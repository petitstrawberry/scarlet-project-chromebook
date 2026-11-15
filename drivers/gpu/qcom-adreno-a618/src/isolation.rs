// SPDX-License-Identifier: GPL-2.0-only
//! Persistent client page tables with transient kernel execution authority.
//!
//! The backend supplies a halted bus for route changes. An error after the
//! first route mutation is deliberately not rolled back: hardware routing may
//! be uncertain, so the caller must stop the GPU and quarantine this owner and
//! all submission backings. Dropping such an owner leaks its domains safely.

use alloc::{sync::Arc, vec::Vec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Range {
    pub iova: u64,
    pub size: u64,
    pub access: u32,
    pub coherent: bool,
    /// Trusted GPU control storage must never gain authority from user aliases.
    pub kernel: bool,
}

pub(super) fn command_mapping_size(bytes: usize, allocation: usize, page_size: usize) -> Result<usize, &'static str> {
    if !page_size.is_power_of_two() || allocation == 0 || allocation % page_size != 0 || bytes > allocation {
        return Err("a618 isolation: invalid command mapping size");
    }
    bytes.max(1).checked_add(page_size - 1).map(|n| n & !(page_size - 1))
        .filter(|size| *size <= allocation).ok_or("a618 isolation: command mapping overflow")
}

/// Small adapter also exercised by host fakes. Stream indices refer to the
/// same immutable stream list in both adapters, never arbitrary user SIDs.
pub(super) trait Domain: Send + Sync {
    fn translate(&self, iova: u64) -> Option<u64>;
    fn fault_snapshot(&self) -> Option<DomainFault> {
        None
    }
    fn pending_context_fault(&self) -> bool {
        self.fault_snapshot().is_some_and(|fault| fault.context_fault_pending)
    }
    fn map(
        &self,
        iova: u64,
        physical: u64,
        len: usize,
        access: u32,
        coherent: bool,
    ) -> Result<(), &'static str>;
    fn unmap(&self, iova: u64, len: usize) -> Result<(), &'static str>;
    fn attach(&self, stream: usize) -> Result<(), &'static str>;
    fn detach(&self, stream: usize) -> Result<(), &'static str>;
    fn suspend(&self) -> Result<(), &'static str> {
        Err("a618 isolation: suspend unsupported")
    }
    fn resume(&self) -> Result<(), &'static str> {
        Err("a618 isolation: resume unsupported")
    }
    fn try_resume(&self) -> Result<bool, &'static str> {
        self.resume().map(|()| true)
    }
}

/// Read-only controller state captured before a failed execution is quiesced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DomainFault {
    pub context_fault_pending: bool,
    pub global_status: u32,
    pub context_status: u32,
    pub fault_address: u64,
    pub syndrome0: u32,
    pub syndrome1: u32,
}

/// Only a positively acknowledged halt (or completed fault shutdown) may
/// authorize a route mutation. A failed phase never produces this proof.
pub(super) struct DmaStopped(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HaltPhase {
    Idle,
    Gx,
    Clients,
    Axi,
}

pub(super) fn halt_for_switch(
    mut phase: impl FnMut(HaltPhase) -> bool,
) -> Result<DmaStopped, &'static str> {
    for step in [
        HaltPhase::Idle,
        HaltPhase::Gx,
        HaltPhase::Clients,
        HaltPhase::Axi,
    ] {
        if !phase(step) {
            return Err("a618 isolation: idle/bus halt timed out");
        }
    }
    Ok(DmaStopped(()))
}

impl DmaStopped {
    /// Backend calls only after both GPU stop and GMU shutdown succeeded.
    pub(super) fn after_fault_shutdown() -> Self {
        Self(())
    }
}

pub(super) struct ExecutionDomain {
    base: Option<Arc<dyn Domain>>,
    private: Option<Arc<dyn Domain>>,
    streams: usize,
    // Set BEFORE the first potentially mutating operation; cleared only once
    // every base route has been restored successfully while DMA is stopped.
    routed: bool,
    update_uncertain: bool,
    retargets: u8,
    authority: Authority,
    // Immutable full-client attachment generation; transient kernel pages live
    // in authority and are updated independently of these persistent mappings.
    client: Option<Arc<Authority>>,
    parked: bool,
}

/// Oldest idle lease first. The caller counts an executing lease against this
/// same bound and stores owned generation pins alongside each domain.
pub(super) const RETAINED_LEASE_LIMIT: usize = 2;

pub(super) struct IdleLeases<T> {
    entries: [Option<T>; RETAINED_LEASE_LIMIT],
}

impl<T> IdleLeases<T> {
    pub(super) const fn new() -> Self {
        Self { entries: [const { None }; RETAINED_LEASE_LIMIT] }
    }

    pub(super) fn is_full(&self) -> bool {
        self.entries.iter().all(Option::is_some)
    }

    pub(super) fn take_matching(&mut self, matches: impl Fn(&T) -> bool) -> Option<T> {
        let index = self.entries.iter().position(|entry| entry.as_ref().is_some_and(&matches))?;
        let entry = self.entries[index].take();
        self.entries[index..].rotate_left(1);
        entry
    }

    pub(super) fn pop_oldest(&mut self) -> Option<T> {
        self.take_matching(|_| true)
    }

    pub(super) fn push_newest(&mut self, entry: T) -> Result<(), T> {
        if let Some(slot) = self.entries.iter_mut().find(|entry| entry.is_none()) {
            *slot = Some(entry);
            Ok(())
        } else {
            Err(entry)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Page {
    pub range: Range,
    pub physical: u64,
}

/// Immutable extents captured from the successful base mapping's inputs.
/// The caller must pin each owning mapping throughout resolution/execution.
#[derive(Clone, Copy, Debug)]
pub(super) struct PhysicalSpan {
    pub iova: u64,
    pub physical: u64,
    pub size: u64,
}

pub(super) struct PinnedTranslations(Vec<PhysicalSpan>);

impl PinnedTranslations {
    pub(super) fn new(mut spans: Vec<PhysicalSpan>, page_size: u64) -> Result<Self, &'static str> {
        if !page_size.is_power_of_two() {
            return Err("a618 isolation: invalid physical granule");
        }
        spans.sort_unstable_by_key(|span| span.iova);
        let mut end = 0;
        for span in &spans {
            if span.size == 0 || (span.iova | span.physical | span.size) & (page_size - 1) != 0
                || span.iova < end || span.physical.checked_add(span.size).is_none()
            {
                return Err("a618 isolation: invalid pinned physical extent");
            }
            end = span.iova.checked_add(span.size)
                .ok_or("a618 isolation: pinned extent overflow")?;
        }
        Ok(Self(spans))
    }

    pub(super) fn translate(&self, iova: u64) -> Option<u64> {
        let index = self.0.partition_point(|span| span.iova <= iova).checked_sub(1)?;
        let span = self.0[index];
        let offset = iova - span.iova;
        (offset < span.size).then(|| span.physical + offset)
    }
}

/// Diagnostic-only indices: containing range, nearest preceding end, nearest
/// following start. Half-open byte ranges deliberately differ from the rounded
/// page authority. Overlapping ranges select the first containing entry.
pub(super) fn nearby_ranges(
    address: u64,
    ranges: impl Iterator<Item = (u64, u64)>,
) -> [Option<usize>; 3] {
    let mut found = [None; 3];
    let mut before_end = 0;
    let mut after_start = u64::MAX;
    for (index, (start, size)) in ranges.enumerate() {
        let Some(end) = start.checked_add(size).filter(|_| size != 0) else {
            continue;
        };
        if start <= address && address < end {
            found[0].get_or_insert(index);
        } else if end <= address && (found[1].is_none() || end > before_end) {
            found[1] = Some(index);
            before_end = end;
        } else if start > address && (found[2].is_none() || start < after_start) {
            found[2] = Some(index);
            after_start = start;
        }
    }
    found
}

/// Re-resolved for every submission while its backing is pinned and the base
/// mapping gate is held, from current owned mapping inputs or base page tables.
/// An idle domain's physical addresses never authorize the next submission.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Authority {
    pages: Vec<Page>,
    streams: usize,
}

impl Authority {
    #[cfg(feature = "submit-profile")]
    pub(super) fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub(super) fn resolve(
        base: &dyn Domain,
        ranges: &[Range],
        page_size: u64,
        streams: usize,
    ) -> Result<Self, &'static str> {
        Self::resolve_with(ranges, page_size, streams, |iova| base.translate(iova))
    }

    pub(super) fn resolve_with(
        ranges: &[Range],
        page_size: u64,
        streams: usize,
        mut translate: impl FnMut(u64) -> Option<u64>,
    ) -> Result<Self, &'static str> {
        if streams == 0 || !page_size.is_power_of_two() || page_size > usize::MAX as u64 {
            return Err("a618 isolation: invalid streams or page size");
        }
        let mut pages: Vec<Page> = Vec::new();
        for range in ranges {
            if range.size == 0 || range.access == 0 || range.access & !3 != 0 {
                return Err("a618 isolation: invalid mapping authority");
            }
            let end = range
                .iova
                .checked_add(range.size)
                .and_then(|end| end.checked_add(page_size - 1))
                .map(|end| end & !(page_size - 1))
                .ok_or("a618 isolation: mapping range overflow")?;
            let start = range.iova & !(page_size - 1);
            let count = usize::try_from((end - start) / page_size)
                .map_err(|_| "a618 isolation: mapping count overflow")?;
            pages
                .try_reserve(count)
                .map_err(|_| "a618 isolation: mapping allocation failed")?;
            for iova in (start..end).step_by(page_size as usize) {
                pages.push(Page {
                    range: Range {
                        iova,
                        size: page_size,
                        ..*range
                    },
                    physical: 0,
                });
            }
        }
        pages.sort_unstable_by_key(|page| page.range.iova);
        let mut index = 0;
        let mut unique = 0;
        while index < pages.len() {
            let mut page = pages[index].range;
            index += 1;
            while index < pages.len() && pages[index].range.iova == page.iova {
                let alias = pages[index].range;
                if alias.coherent != page.coherent
                    || alias.kernel != page.kernel
                    || (page.kernel && alias.access != page.access)
                {
                    return Err("a618 isolation: conflicting kernel page authority");
                }
                // Caller-owned buffers commonly share an allocation page
                // between uniforms, descriptors and writable SSBO/CCU data.
                // The page permission is the union of declared authorities;
                // this never imports a page from another resource/context.
                page.access |= alias.access;
                index += 1;
            }
            let physical = translate(page.iova)
                .filter(|physical| physical & (page_size - 1) == 0)
                .ok_or("a618 isolation: missing physical page")?;
            pages[unique] = Page {
                range: page,
                physical,
            };
            unique += 1;
        }
        pages.truncate(unique);
        Ok(Self { pages, streams })
    }
}

/// At most four private context banks, including the active domain removed
/// from this cache. Entries are oldest first; none is attached to any stream.
/// No resource Arc is retained: the current request pins its own backing and
/// rechecks physical identity before a hit, avoiding destructor lock inversion.
#[cfg(test)]
pub(super) struct DomainCache {
    entries: [Option<ExecutionDomain>; 4],
}

#[cfg(test)]
impl DomainCache {
    pub(super) const fn new() -> Self {
        Self {
            entries: [const { None }; 4],
        }
    }

    fn take(&mut self, index: usize) -> Option<ExecutionDomain> {
        let owner = self.entries[index].take();
        self.entries[index..].rotate_left(1);
        owner
    }

    pub(super) fn take_matching(&mut self, authority: &Authority) -> Option<ExecutionDomain> {
        let index = self.entries.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|owner| !owner.routed && !owner.update_uncertain && owner.authority == *authority)
        })?;
        self.take(index)
    }

    /// Keep the four-bank bound. Recycle a bank after at most 64 differential
    /// updates because SMMUv2 unmap retains empty intermediate page tables.
    /// Exact hits remain eligible indefinitely (they allocate no new tables).
    pub(super) fn take_retargetable(&mut self, authority: &Authority) -> Option<ExecutionDomain> {
        if self.entries.iter().any(Option::is_none) {
            return None; // warm the bounded exact-match cache first
        }
        let index = self.entries.iter().position(|entry| entry.as_ref().is_some_and(|owner| {
            !owner.routed && !owner.update_uncertain && owner.retargets < 64
                && owner.authority.streams == authority.streams
        }))?;
        self.take(index)
    }

    /// Release an idle context bank before allocation, also usable when another
    /// device has consumed banks and the normal allocation attempt fails.
    pub(super) fn evict_oldest(&mut self) -> bool {
        if let Some(index) = self.entries.iter().position(Option::is_some) {
            drop(self.take(index));
            true
        } else {
            false
        }
    }

    pub(super) fn make_room(&mut self) {
        if self.entries.iter().all(Option::is_some) {
            self.evict_oldest();
        }
    }

    pub(super) fn retain_restored(&mut self, owner: ExecutionDomain) -> bool {
        if owner.routed || owner.update_uncertain {
            // A partial route failure retains the domains via ExecutionDomain
            // Drop; such an owner can never become an eligible cache hit.
            return false;
        }
        self.make_room();
        let index = self.entries.iter().position(Option::is_none).unwrap();
        self.entries[index] = Some(owner);
        true
    }
}

impl ExecutionDomain {
    pub(super) fn ownership_uncertain(&self) -> bool {
        self.routed || self.update_uncertain
    }
    #[cfg(feature = "submit-profile")]
    pub(super) fn kernel_matches(&self, authority: &Authority) -> bool {
        self.authority == *authority
    }

    /// Change only idle, detached page tables. Every removed/replaced page is
    /// unmapped and synchronized before any new mapping is installed. An
    /// interrupted update is never routed or cached, and retains its tables.
    pub(super) fn retarget(&mut self, authority: Authority) -> Result<(), &'static str> {
        if let Some(client) = &self.client {
            Self::check_kernel_authority(client, &authority)?;
        }
        if self.routed
            || self.update_uncertain
            || (self.client.is_none() && self.retargets >= 64)
            || self.streams != authority.streams
        {
            return Err("a618 isolation: unsafe domain retarget");
        }
        if self.pending_bind_fault().is_some() {
            return Err("a618 isolation: pending IOMMU fault before retarget");
        }
        let removed = changed_pages(&self.authority.pages, &authority.pages)?;
        let added = changed_pages(&authority.pages, &self.authority.pages)?;
        self.update_uncertain = true;
        let private = self.private.as_deref().unwrap();
        let mut index = 0;
        while index < removed.len() {
            let first = removed[index];
            let mut end = first.range.iova + first.range.size;
            index += 1;
            while index < removed.len() && removed[index].range.iova == end {
                end += removed[index].range.size;
                index += 1;
            }
            private.unmap(first.range.iova, usize::try_from(end - first.range.iova)
                .map_err(|_| "a618 isolation: unmap length overflow")?)?;
        }
        map_pages(private, &added)?;
        self.authority = authority;
        self.retargets = self.retargets.saturating_add(1);
        self.update_uncertain = false;
        Ok(())
    }
    /// A cached domain's old fault must be attributed before any route switch,
    /// not erased to make a new job runnable. The bool identifies the private
    /// bank; a false value identifies the currently selected base bank.
    pub(super) fn pending_bind_fault(&self) -> Option<(bool, Option<DomainFault>)> {
        if self.routed {
            return None; // A partial switch needs the existing quarantine path.
        }
        for (private, domain) in [(true, &self.private), (false, &self.base)] {
            let domain = domain.as_deref().unwrap();
            if domain.pending_context_fault() {
                return Some((private, domain.fault_snapshot()));
            }
        }
        None
    }

    /// The accepted page authority, not a new base-domain authorization. No
    /// mapping/cache mutation or allocation is permitted during this lookup.
    pub(super) fn diagnostic_pages(&self, address: u64) -> [Option<Page>; 3]{
        let pages = || {
            self.authority
                .pages
                .iter()
                .chain(self.client.iter().flat_map(|client| client.pages.iter()))
        };
        nearby_ranges(
            address,
            pages().map(|page| (page.range.iova, page.range.size)),
        )
        .map(|index| index.and_then(|index| pages().nth(index).copied()))
    }

    /// Diagnostics must follow the execution route, not the base mapping used
    /// to build it. A failed partial switch may have uncertain routing; this
    /// accessor is not a route or retirement proof.
    pub(super) fn diagnostic_domain(&self) -> &dyn Domain {
        if self.routed {
            self.private.as_deref().unwrap()
        } else {
            self.base.as_deref().unwrap()
        }
    }

    #[cfg(test)]
    pub(super) fn prepare(
        base: Arc<dyn Domain>,
        private: Arc<dyn Domain>,
        streams: usize,
        ranges: &[Range],
        page_size: u64,
    ) -> Result<Self, &'static str> {
        let authority = Authority::resolve(base.as_ref(), ranges, page_size, streams)?;
        Self::from_authority(base, private, authority)
    }

    pub(super) fn from_authority(
        base: Arc<dyn Domain>,
        private: Arc<dyn Domain>,
        authority: Authority,
    ) -> Result<Self, &'static str> {
        map_pages(private.as_ref(), &authority.pages)?;
        Ok(Self {
            base: Some(base), private: Some(private), streams: authority.streams,
            routed: false, update_uncertain: false, retargets: 0, authority, client: None, parked: false,
        })
    }

    fn check_kernel_authority(client: &Authority, kernel: &Authority) -> Result<(), &'static str> {
        if client.streams != kernel.streams
            || kernel.pages.iter().any(|page| {
                !page.range.kernel
                    || client
                        .pages
                        .binary_search_by_key(&page.range.iova, |p| p.range.iova)
                        .is_ok()
            })
        {
            return Err("a618 isolation: client/kernel authority overlap");
        }
        Ok(())
    }

    /// Map the client's complete attached allocation generation once. Callers
    /// must keep real backing pins alive as long as this owner, even when idle.
    pub(super) fn from_client_authority(
        base: Arc<dyn Domain>,
        private: Arc<dyn Domain>,
        client: Arc<Authority>,
        kernel: Authority,
    ) -> Result<Self, &'static str> {
        if client.pages.iter().any(|page| page.range.kernel) {
            return Err("a618 isolation: privileged client mapping");
        }
        Self::check_kernel_authority(&client, &kernel)?;
        map_pages(private.as_ref(), &client.pages)?;
        let mut owner = Self::from_authority(base, private, kernel)?;
        owner.client = Some(client);
        Ok(owner)
    }

    /// The provider explicitly allocated this domain without a hardware lease.
    pub(super) fn from_parked_client_authority(
        base: Arc<dyn Domain>,
        private: Arc<dyn Domain>,
        client: Arc<Authority>,
        kernel: Authority,
    ) -> Result<Self, &'static str> {
        let mut owner = Self::from_client_authority(base, private, client, kernel)?;
        owner.parked = true;
        Ok(owner)
    }

    /// Base routes were restored under the supplied halt proof. Keeping the
    /// detached lease never grants authority to a different client generation.
    pub(super) fn retain_idle(&self, _stopped: &DmaStopped) -> Result<(), &'static str> {
        if self.routed || self.update_uncertain || self.parked || self.pending_bind_fault().is_some() {
            return Err("a618 isolation: unsafe idle lease retention");
        }
        Ok(())
    }

    /// Called only after verified halt and complete restoration of base routes.
    /// Retain page tables but release the scarce context bank for other clients.
    pub(super) fn suspend_idle(&mut self, _stopped: &DmaStopped) -> Result<(), &'static str> {
        if self.routed || self.update_uncertain || self.pending_bind_fault().is_some() {
            return Err("a618 isolation: unsafe VM suspend");
        }
        self.update_uncertain = true;
        self.private.as_ref().unwrap().suspend()?;
        self.parked = true;
        self.update_uncertain = false;
        Ok(())
    }

    pub(super) fn resume_idle(&mut self) -> Result<(), &'static str> {
        if self.try_resume_idle()? {
            Ok(())
        } else {
            Err("a618 isolation: no idle context bank available")
        }
    }

    pub(super) fn try_resume_idle(&mut self) -> Result<bool, &'static str> {
        if self.routed || self.update_uncertain || self.pending_bind_fault().is_some() {
            return Err("a618 isolation: unsafe VM resume");
        }
        if !self.parked {
            return Ok(true); // Exact-generation resident lease.
        }
        self.update_uncertain = true;
        if !self.private.as_ref().unwrap().try_resume()? {
            // False is permitted only before any domain configuration changed.
            self.update_uncertain = false;
            return Ok(false);
        }
        self.parked = false;
        if self.private.as_ref().unwrap().pending_context_fault() {
            return Err("a618 isolation: pending IOMMU fault after VM resume");
        }
        self.update_uncertain = false;
        Ok(true)
    }

    /// Caller must have positively acknowledged a bounded GX/client/AXI halt
    /// and keep that halt asserted until all routing operations complete.
    pub(super) fn enter_halted(&mut self, _stopped: &DmaStopped) -> Result<(), &'static str> {
        if self.routed || self.update_uncertain || self.parked {
            return Err("a618 isolation: domain already routed or update uncertain");
        }
        if self.pending_bind_fault().is_some() {
            return Err("a618 isolation: pending IOMMU fault before bind");
        }
        self.routed = true;
        for stream in 0..self.streams {
            self.base.as_ref().unwrap().detach(stream)?;
            self.private.as_ref().unwrap().attach(stream)?;
        }
        Ok(())
    }

    /// Also requires DMA stopped. Never invoked after an uncertain route error.
    pub(super) fn restore_halted(&mut self, _stopped: &DmaStopped) -> Result<(), &'static str> {
        if !self.routed || self.update_uncertain {
            return Err("a618 isolation: private domain is not active");
        }
        for stream in 0..self.streams {
            self.private.as_ref().unwrap().detach(stream)?;
            self.base.as_ref().unwrap().attach(stream)?;
        }
        self.routed = false;
        Ok(())
    }
}

fn changed_pages(from: &[Page], to: &[Page]) -> Result<Vec<Page>, &'static str> {
    let mut changed = Vec::new();
    let mut next = 0;
    for page in from {
        while next < to.len() && to[next].range.iova < page.range.iova {
            next += 1;
        }
        if to.get(next) != Some(page) {
            changed.try_reserve(1).map_err(|_| "a618 isolation: delta allocation failed")?;
            changed.push(*page);
        }
    }
    Ok(changed)
}

fn map_pages(private: &dyn Domain, pages: &[Page]) -> Result<(), &'static str> {
        let mut index = 0;
        while index < pages.len() {
            let page = pages[index];
            let mut len = usize::try_from(page.range.size)
                .map_err(|_| "a618 isolation: mapping length overflow")?;
            index += 1;
            // Authority has already checked/merged aliases page by page. Only
            // adjacent virtual AND physical pages with identical authority may
            // share a map call. The IOMMU still maps every page but synchronizes
            // once per contiguous run, rather than once per 4 KiB page.
            while let Some(next) = pages.get(index) {
                if page.range.iova.checked_add(len as u64) != Some(next.range.iova)
                    || page.physical.checked_add(len as u64) != Some(next.physical)
                    || next.range.access != page.range.access
                    || next.range.coherent != page.range.coherent
                    || next.range.kernel != page.range.kernel
                    || next.range.size != page.range.size
                {
                    break;
                }
                let Some(extended) = len.checked_add(page.range.size as usize) else {
                    break;
                };
                len = extended;
                index += 1;
            }
            private.map(
                page.range.iova,
                page.physical,
                len,
                page.range.access,
                page.range.coherent,
            )?;
        }
        Ok(())
}

impl Drop for ExecutionDomain {
    fn drop(&mut self) {
        if self.routed || self.update_uncertain {
            core::mem::forget(self.private.take());
            core::mem::forget(self.base.take());
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{collections::BTreeMap, sync::Mutex, vec};

    fn client_vm(base: Arc<Fake>, private: Arc<Fake>) -> ExecutionDomain {
        // Full attached allocation, even if a particular job references fewer
        // bytes. 0x9000 belongs to another client and never enters this VM.
        let client = Arc::new(Authority::resolve(base.as_ref(), &[range()], 4096, 1).unwrap());
        let kernel = Authority::resolve(
            base.as_ref(),
            &[Range {
                iova: 0x5000,
                size: 8192,
                access: 1,
                coherent: false,
                kernel: true,
            }],
            4096,
            1,
        )
        .unwrap();
        ExecutionDomain::from_client_authority(base, private, client, kernel).unwrap()
    }

    fn vm_base() -> Arc<Fake> {
        let base = base();
        base.map(0x5000, 0x50000, 8192, 1, false).unwrap();
        base
    }

    #[test]
    fn persistent_client_maps_once_across_many_jobs_and_ib_tail_changes() {
        let base = vm_base();
        let private = Arc::new(Fake::default());
        let mut vm = client_vm(base.clone(), private.clone());
        let stopped = halt_for_switch(|_| true).unwrap();
        for index in 0..128 {
            vm.enter_halted(&stopped).unwrap();
            assert!(vm.suspend_idle(&stopped).is_err()); // still routed
            vm.restore_halted(&stopped).unwrap();
            vm.suspend_idle(&stopped).unwrap();
            assert!(vm.enter_halted(&stopped).is_err()); // bank is parked
            let kernel = Authority::resolve(
                base.as_ref(),
                &[Range {
                    iova: 0x5000,
                    size: if index % 2 == 0 { 4096 } else { 8192 },
                    access: 1,
                    coherent: false,
                    kernel: true,
                }],
                4096,
                1,
            )
            .unwrap();
            vm.retarget(kernel).unwrap();
            assert_eq!(private.translate(0x6000).is_some(), index % 2 != 0);
            vm.resume_idle().unwrap();
        }
        assert_eq!(
            private
                .maps
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.0 < 0x5000)
                .count(),
            2
        );
        assert!(
            private
                .unmaps
                .lock()
                .unwrap()
                .iter()
                .all(|call| call.0 >= 0x5000)
        );
        assert_eq!(
            private.pages.lock().unwrap().get(&0x1000),
            Some(&(0x10000, 1, true))
        );
        assert_eq!(private.translate(0x9000), None);
        assert_eq!(vm.diagnostic_pages(0x1000)[0].unwrap().physical, 0x10000);
    }

    #[test]
    fn many_idle_client_tables_release_each_bank_and_rebind_without_remapping() {
        let base = vm_base();
        let stopped = halt_for_switch(|_| true).unwrap();
        let mut clients = Vec::new();
        for _ in 0..32 {
            let private = Arc::new(Fake::default());
            let mut vm = client_vm(base.clone(), private.clone());
            vm.enter_halted(&stopped).unwrap();
            vm.restore_halted(&stopped).unwrap();
            vm.suspend_idle(&stopped).unwrap();
            assert!(vm.parked);
            assert_eq!(*private.power.lock().unwrap(), [false]);
            clients.push((vm, private));
        }
        for (mut vm, private) in clients {
            let maps = private.maps.lock().unwrap().len();
            vm.resume_idle().unwrap();
            vm.enter_halted(&stopped).unwrap();
            vm.restore_halted(&stopped).unwrap();
            vm.suspend_idle(&stopped).unwrap();
            assert_eq!(*private.power.lock().unwrap(), [false, true, false]);
            assert_eq!(private.maps.lock().unwrap().len(), maps);
        }
    }

    #[test]
    fn persistent_kernel_update_cannot_replace_or_upgrade_client_pages() {
        let base = vm_base();
        let private = Arc::new(Fake::default());
        let mut vm = client_vm(base.clone(), private.clone());
        let calls = private.maps.lock().unwrap().len();
        for kernel in [false, true] {
            let bad = Authority::resolve(
                base.as_ref(),
                &[Range {
                    iova: 0x1000,
                    size: 4096,
                    access: 3,
                    kernel,
                    ..range()
                }],
                4096,
                1,
            )
            .unwrap();
            assert!(vm.retarget(bad).is_err());
        }
        assert_eq!(private.maps.lock().unwrap().len(), calls);
        assert!(private.unmaps.lock().unwrap().is_empty());
        assert_eq!(
            private.pages.lock().unwrap().get(&0x1000),
            Some(&(0x10000, 1, true))
        );
    }

    #[test]
    fn park_or_resume_failure_prevents_rebind_and_quarantines_tables() {
        let stopped = halt_for_switch(|_| true).unwrap();
        for fail_resume in [false, true] {
            let private = Arc::new(Fake {
                fail_suspend: !fail_resume,
                fail_resume,
                ..Fake::default()
            });
            let mut vm = client_vm(vm_base(), private.clone());
            vm.enter_halted(&stopped).unwrap();
            vm.restore_halted(&stopped).unwrap();
            if fail_resume {
                vm.suspend_idle(&stopped).unwrap();
                assert!(vm.resume_idle().is_err());
            } else {
                assert!(vm.suspend_idle(&stopped).is_err());
            }
            assert!(vm.update_uncertain);
            let routes = private.events.lock().unwrap().len();
            assert!(vm.enter_halted(&stopped).is_err());
            assert_eq!(private.events.lock().unwrap().len(), routes);
            drop(vm);
            assert_eq!(Arc::strong_count(&private), 2);
        }
    }

    #[test]
    fn pending_fault_is_preserved_before_bank_release() {
        let private = Arc::new(Fake::default());
        let mut vm = client_vm(vm_base(), private.clone());
        let fault = DomainFault {
            context_fault_pending: true,
            global_status: 0,
            context_status: 0x402,
            fault_address: 0x1115b3000,
            syndrome0: 3,
            syndrome1: 4,
        };
        *private.fault.lock().unwrap() = Some(fault);
        assert!(
            vm.suspend_idle(&halt_for_switch(|_| true).unwrap())
                .is_err()
        );
        assert!(private.power.lock().unwrap().is_empty());
        assert_eq!(private.fault_snapshot(), Some(fault));
    }

    #[test]
    fn command_authority_shrinks_to_active_ib_pages_and_removes_stale_tail() {
        let size = command_mapping_size(2838 * 4, 2 * 1024 * 1024, 4096).unwrap();
        assert_eq!(size, 3 * 4096);
        assert_eq!(command_mapping_size(0, 8192, 4096), Ok(4096));
        assert_eq!(command_mapping_size(4096, 8192, 4096), Ok(4096));
        assert!(command_mapping_size(8193, 8192, 4096).is_err());
        assert!(command_mapping_size(1, 8192, 0).is_err());
        let base = Arc::new(Fake::default());
        base.map(0x200000, 0x800000, 2 * 1024 * 1024, 1, false).unwrap();
        let range = Range { iova: 0x200000, size: 2 * 1024 * 1024, access: 1, coherent: false, kernel: true };
        let private = Arc::new(Fake::default());
        let mut owner = ExecutionDomain::prepare(base.clone(), private.clone(), 1, &[range], 4096).unwrap();
        owner.retarget(Authority::resolve(base.as_ref(), &[Range { size: size as u64, ..range }], 4096, 1).unwrap()).unwrap();
        assert_eq!(private.pages.lock().unwrap().len(), 3);
        assert_eq!(private.translate(0x203000), None);
        assert_eq!(*private.unmaps.lock().unwrap(), [(0x203000, 2 * 1024 * 1024 - size)]);
    }

    #[test]
    fn pinned_extents_match_segmented_walk_and_reject_missing_or_recycled_pages() {
        let base = base();
        let spans = vec![
            PhysicalSpan { iova: 0x2000, physical: 0x30000, size: 4096 },
            PhysicalSpan { iova: 0x1000, physical: 0x10000, size: 4096 },
        ];
        let pinned = PinnedTranslations::new(spans.clone(), 4096).unwrap();
        let authority = Authority::resolve_with(&[range()], 4096, 1, |a| pinned.translate(a)).unwrap();
        assert_eq!(authority, Authority::resolve(base.as_ref(), &[range()], 4096, 1).unwrap());
        assert_eq!(pinned.translate(0x2fff), Some(0x30fff));
        assert_eq!(pinned.translate(0x3000), None);
        assert_eq!(pinned.translate(0xfff), None);
        assert!(PinnedTranslations::new(vec![spans[0], spans[0]], 4096).is_err());
        let missing = PinnedTranslations::new(vec![spans[0]], 4096).unwrap();
        assert!(Authority::resolve_with(&[range()], 4096, 1, |a| missing.translate(a)).is_err());
        let mut recycled = spans;
        recycled[1].physical = 0x50000;
        let recycled = PinnedTranslations::new(recycled, 4096).unwrap();
        let changed = Authority::resolve_with(&[range()], 4096, 1, |a| recycled.translate(a)).unwrap();
        assert_ne!(authority, changed); // same IOVA, new pinned mapping identity
    }

    #[test]
    fn delta_keeps_large_image_and_maps_only_changed_uniform_page() {
        let base = Arc::new(Fake::default());
        base.map(0x200000, 0x800000, 2 * 1024 * 1024, 3, true).unwrap();
        base.map(0x600000, 0xc00000, 4096, 3, true).unwrap();
        base.map(0x601000, 0xd00000, 4096, 3, true).unwrap();
        let image = Range { iova: 0x200000, size: 2 * 1024 * 1024, access: 1, coherent: true, kernel: false };
        let uniform = Range { iova: 0x600000, size: 4096, ..image };
        let private = Arc::new(Fake::default());
        let mut owner = ExecutionDomain::prepare(base.clone(), private.clone(), 1, &[image, uniform], 4096).unwrap();
        let initial_maps = private.maps.lock().unwrap().len();
        let next = Range { iova: 0x601000, ..uniform };
        let authority = Authority::resolve(base.as_ref(), &[image, next], 4096, 1).unwrap();
        owner.retarget(authority).unwrap();
        assert_eq!(private.maps.lock().unwrap().len(), initial_maps + 1);
        assert_eq!(*private.unmaps.lock().unwrap(), [(0x600000, 4096)]);
        let pages = private.pages.lock().unwrap();
        assert_eq!(pages.len(), 513);
        assert!(!pages.contains_key(&0x600000));
        assert_eq!(pages.get(&0x601000), Some(&(0xd00000, 1, true)));
        assert_eq!(pages.get(&0x200000), Some(&(0x800000, 1, true)));
        assert!(private.events.lock().unwrap().is_empty());
    }

    #[test]
    fn delta_removes_old_pages_and_replaces_physical_permissions_and_coherency() {
        let base = base();
        let private = Arc::new(Fake::default());
        let mut owner = ExecutionDomain::prepare(base.clone(), private.clone(), 1,
            &[Range { access: 3, ..range() }], 4096).unwrap();
        base.pages.lock().unwrap().get_mut(&0x1000).unwrap().0 = 0x50000;
        let next = Range { size: 128, access: 1, coherent: false, ..range() };
        owner.retarget(Authority::resolve(base.as_ref(), &[next], 4096, 1).unwrap()).unwrap();
        assert_eq!(*private.unmaps.lock().unwrap(), [(0x1000, 8192)]);
        assert_eq!(*private.pages.lock().unwrap(), BTreeMap::from([(0x1000, (0x50000, 1, false))]));
        assert_eq!(owner.authority.pages.len(), 1);
    }

    #[test]
    fn failed_delta_never_binds_or_reenters_cache_and_keeps_domains() {
        for fail_unmap in [false, true] {
            let base = base();
            let private = Arc::new(Fake { fail_unmap, fail_map_after: Some(2), ..Fake::default() });
            let mut owner = ExecutionDomain::prepare(base.clone(), private.clone(), 1, &[range()], 4096).unwrap();
            let authority = Authority::resolve(base.as_ref(), &[Range { access: 3, ..range() }], 4096, 1).unwrap();
            assert!(owner.retarget(authority).is_err());
            assert!(owner.update_uncertain);
            assert!(owner.enter_halted(&halt_for_switch(|_| true).unwrap()).is_err());
            let mut cache = DomainCache::new();
            assert!(!cache.retain_restored(owner));
            assert!(private.events.lock().unwrap().is_empty());
            assert_eq!(Arc::strong_count(&private), 2);
        }
    }

    #[test]
    fn faulted_or_routed_bank_cannot_be_retargeted() {
        let base = base();
        let private = Arc::new(Fake::default());
        let mut owner = ExecutionDomain::prepare(base.clone(), private.clone(), 1, &[range()], 4096).unwrap();
        let fault = DomainFault { context_fault_pending: true, global_status: 0, context_status: 0x402,
            fault_address: 0x1000, syndrome0: 2, syndrome1: 3 };
        *private.fault.lock().unwrap() = Some(fault);
        assert!(owner.retarget(Authority::resolve(base.as_ref(), &[range()], 4096, 1).unwrap()).is_err());
        assert_eq!(*private.fault.lock().unwrap(), Some(fault));
        assert!(private.unmaps.lock().unwrap().is_empty());
        assert!(!owner.update_uncertain);
        *private.fault.lock().unwrap() = None;
        owner.enter_halted(&halt_for_switch(|_| true).unwrap()).unwrap();
        assert!(owner.retarget(Authority::resolve(base.as_ref(), &[range()], 4096, 1).unwrap()).is_err());
        owner.restore_halted(&halt_for_switch(|_| true).unwrap()).unwrap();
    }

    #[test]
    fn delta_cache_preserves_four_bank_bound_and_limits_table_growth() {
        let base = base();
        let mut cache = DomainCache::new();
        for _ in 0..4 {
            let mut owner = ExecutionDomain::prepare(base.clone(), Arc::new(Fake::default()), 1, &[range()], 4096).unwrap();
            owner.retargets = 64;
            cache.retain_restored(owner);
        }
        let changed = Authority::resolve(base.as_ref(), &[Range { access: 3, ..range() }], 4096, 1).unwrap();
        assert!(cache.take_retargetable(&changed).is_none());
        cache.entries[1].as_mut().unwrap().retargets = 63;
        let mut owner = cache.take_retargetable(&changed).unwrap();
        owner.retarget(changed).unwrap();
        assert_eq!(owner.retargets, 64);
        assert!(cache.retain_restored(owner));
        assert_eq!(cache.entries.iter().flatten().count(), 4);
    }
    #[derive(Default)]
    struct Fake {
        pages: Mutex<BTreeMap<u64, (u64, u32, bool)>>,
        events: Mutex<Vec<(bool, usize)>>,
        maps: Mutex<Vec<(u64, u64, usize, u32, bool)>>,
        fail_map: bool,
        fail_map_after: Option<usize>,
        fail_unmap: bool,
        unmaps: Mutex<Vec<(u64, usize)>>,
        fail_route: Option<(bool, usize)>,
        fault: Mutex<Option<DomainFault>>,
        power: Mutex<Vec<bool>>,
        fail_suspend: bool,
        fail_resume: bool,
    }
    impl Domain for Fake {
        fn fault_snapshot(&self) -> Option<DomainFault> {
            *self.fault.lock().unwrap()
        }
        fn suspend(&self) -> Result<(), &'static str> {
            self.power.lock().unwrap().push(false);
            if self.fail_suspend {
                Err("suspend")
            } else {
                Ok(())
            }
        }
        fn resume(&self) -> Result<(), &'static str> {
            self.power.lock().unwrap().push(true);
            if self.fail_resume {
                Err("resume")
            } else {
                Ok(())
            }
        }
        fn translate(&self, iova: u64) -> Option<u64> {
            self.pages.lock().unwrap().get(&iova).map(|p| p.0)
        }
        fn map(
            &self,
            iova: u64,
            p: u64,
            len: usize,
            access: u32,
            coherent: bool,
        ) -> Result<(), &'static str> {
            let mut calls = self.maps.lock().unwrap();
            let call = calls.len();
            calls.push((iova, p, len, access, coherent));
            if self.fail_map || self.fail_map_after == Some(call) {
                return Err("map");
            }
            assert_eq!(len % 4096, 0);
            for offset in (0..len).step_by(4096) {
                assert!(
                    self.pages
                        .lock()
                        .unwrap()
                        .insert(iova + offset as u64, (p + offset as u64, access, coherent))
                        .is_none()
                );
            }
            Ok(())
        }
        fn unmap(&self, iova: u64, len: usize) -> Result<(), &'static str> {
            self.unmaps.lock().unwrap().push((iova, len));
            if self.fail_unmap { return Err("unmap"); }
            for offset in (0..len).step_by(4096) {
                assert!(self.pages.lock().unwrap().remove(&(iova + offset as u64)).is_some());
            }
            Ok(())
        }
        fn attach(&self, stream: usize) -> Result<(), &'static str> {
            self.route(true, stream)
        }
        fn detach(&self, stream: usize) -> Result<(), &'static str> {
            self.route(false, stream)
        }
    }
    impl Fake {
        fn route(&self, attach: bool, stream: usize) -> Result<(), &'static str> {
            self.events.lock().unwrap().push((attach, stream));
            if self.fail_route == Some((attach, stream)) {
                Err("route")
            } else {
                Ok(())
            }
        }
    }
    fn base() -> Arc<Fake> {
        let base = Arc::new(Fake::default());
        base.map(0x1000, 0x10000, 4096, 3, true).unwrap();
        base.map(0x2000, 0x30000, 4096, 3, true).unwrap(); // segmented physical backing
        base.map(0x9000, 0x90000, 4096, 3, true).unwrap(); // another context
        base
    }
    fn range() -> Range {
        Range {
            iova: 0x1080,
            size: 0x1000,
            access: 1,
            coherent: true,
            kernel: false,
        }
    }

    #[test]
    fn fault_range_lookup_preserves_half_open_boundaries_and_wire_indices() {
        let ranges = [(0x3000, 0x1000), (0x1004, 0x1000), (0x2000, 0x100)];
        let locate = |address| nearby_ranges(address, ranges.into_iter());
        assert_eq!(locate(0x1000), [None, None, Some(1)]);
        assert_eq!(locate(0x1004), [Some(1), None, Some(2)]);
        assert_eq!(locate(0x2003), [Some(1), None, Some(0)]);
        assert_eq!(locate(0x2004), [Some(2), Some(1), Some(0)]);
        assert_eq!(locate(0x2100), [None, Some(2), Some(0)]);
        assert_eq!(locate(0x4000), [None, Some(0), None]);
        assert_eq!(nearby_ranges(u64::MAX, [(1, 0), (u64::MAX, 2)].into_iter()), [None; 3]);
    }

    #[test]
    fn fault_pages_distinguish_wire_padding_and_unsubmitted_base_resources() {
        let base = base();
        let private = Arc::new(Fake::default());
        let owner = ExecutionDomain::prepare(base.clone(), private.clone(), 1, &[range()], 4096)
            .unwrap();
        // Page padding is mapped, although the byte range starts at 0x1080.
        let page = owner.diagnostic_pages(0x1000)[0].unwrap();
        assert_eq!(page.range.iova, 0x1000);
        assert_eq!(page.range.access, 1);
        assert_eq!(page.physical, 0x10000);
        assert_eq!(nearby_ranges(0x1000, [(range().iova, range().size)].into_iter())[0], None);
        // A base mapping from a different context must not become authority.
        assert_eq!(base.translate(0x9000), Some(0x90000));
        let pages = owner.diagnostic_pages(0x9000);
        assert_eq!(pages[0], None);
        assert_eq!(pages[1].unwrap().range.iova, 0x2000);
        assert_eq!(pages[2], None);
        assert_eq!(private.translate(0x9000), None);
        assert!(base.events.lock().unwrap().is_empty());
        assert!(private.events.lock().unwrap().is_empty());
    }

    #[test]
    fn fault_far_at_readonly_range_end_is_outside_authority() {
        let far = 0x10ec0b000;
        let ranges = [(far - 0x40000, 0x40000), (far + 0x1000, 0x40000)];
        assert_eq!(nearby_ranges(far, ranges.into_iter()), [None, Some(0), Some(1)]);
        assert_eq!(nearby_ranges(far - 1, ranges.into_iter())[0], Some(0));
    }

    #[test]
    fn contiguous_two_megabytes_map_once_with_identical_page_authority() {
        let base = Arc::new(Fake::default());
        let private = Arc::new(Fake::default());
        let size = 2 * 1024 * 1024;
        base.map(0x200000, 0x800000, size, 3, false).unwrap();
        let ranges = [Range {
            iova: 0x200000,
            size: size as u64,
            access: 1,
            coherent: false,
            kernel: true,
        }];
        let mut owner =
            ExecutionDomain::prepare(base.clone(), private.clone(), 1, &ranges, 4096).unwrap();
        assert_eq!(
            *private.maps.lock().unwrap(),
            vec![(0x200000, 0x800000, size, 1, false)]
        );
        let expected = (0..size)
            .step_by(4096)
            .map(|offset| {
                (
                    0x200000 + offset as u64,
                    (0x800000 + offset as u64, 1, false),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(*private.pages.lock().unwrap(), expected);
        owner
            .enter_halted(&halt_for_switch(|_| true).unwrap())
            .unwrap();
        owner
            .restore_halted(&halt_for_switch(|_| true).unwrap())
            .unwrap();
        let mut cache = DomainCache::new();
        assert!(cache.retain_restored(owner));
        let authority = Authority::resolve(base.as_ref(), &ranges, 4096, 1).unwrap();
        assert!(cache.take_matching(&authority).is_some());
        assert_eq!(private.maps.lock().unwrap().len(), 1);
    }

    #[test]
    fn coalescing_stops_at_every_address_or_authority_boundary() {
        for boundary in 0..5 {
            let base = Arc::new(Fake::default());
            let private = Arc::new(Fake::default());
            let first = Range {
                iova: 0x1000,
                size: 4096,
                ..range()
            };
            let second = Range {
                iova: if boundary == 0 { 0x3000 } else { 0x2000 },
                access: if boundary == 2 { 3 } else { first.access },
                coherent: if boundary == 3 { false } else { first.coherent },
                kernel: boundary == 4,
                ..first
            };
            let second_physical = if boundary == 1 { 0x30000 } else { 0x11000 };
            base.map(first.iova, 0x10000, 4096, 3, true).unwrap();
            base.map(second.iova, second_physical, 4096, 3, true)
                .unwrap();
            let _owner =
                ExecutionDomain::prepare(base, private.clone(), 1, &[second, first], 4096).unwrap();
            assert_eq!(
                *private.maps.lock().unwrap(),
                vec![
                    (first.iova, 0x10000, 4096, first.access, first.coherent),
                    (
                        second.iova,
                        second_physical,
                        4096,
                        second.access,
                        second.coherent
                    ),
                ]
            );
            assert_eq!(private.pages.lock().unwrap().len(), 2);
        }
    }

    #[test]
    fn failed_later_run_never_routes_partially_mapped_domain() {
        let base = base(); // two physically disjoint runs
        let private = Arc::new(Fake {
            fail_map_after: Some(1),
            ..Fake::default()
        });
        assert!(
            ExecutionDomain::prepare(base.clone(), private.clone(), 1, &[range()], 4096,).is_err()
        );
        assert_eq!(private.maps.lock().unwrap().len(), 2);
        assert_eq!(private.pages.lock().unwrap().len(), 1);
        assert!(private.events.lock().unwrap().is_empty());
        assert!(base.events.lock().unwrap().is_empty());
        assert_eq!(Arc::strong_count(&private), 1);
    }

    #[test]
    fn cache_reuses_four_exact_domains_without_mapping_again() {
        let base = base();
        let mut cache = DomainCache::new();
        let mut privates = Vec::new();
        for iteration in 0..128 {
            let variant = iteration % 4;
            let ranges = [Range {
                access: if variant & 1 == 0 { 1 } else { 3 },
                coherent: variant & 2 == 0,
                ..range()
            }];
            let authority = Authority::resolve(base.as_ref(), &ranges, 4096, 2).unwrap();
            let mut owner = match cache.take_matching(&authority) {
                Some(owner) => {
                    assert!(iteration >= 4);
                    owner
                }
                None => {
                    assert!(iteration < 4, "warm domain must hit");
                    let private = Arc::new(Fake::default());
                    privates.push(private.clone());
                    ExecutionDomain::from_authority(base.clone(), private, authority).unwrap()
                }
            };
            // Reused domains must still perform both complete route changes.
            owner
                .enter_halted(&halt_for_switch(|_| true).unwrap())
                .unwrap();
            owner
                .restore_halted(&halt_for_switch(|_| true).unwrap())
                .unwrap();
            assert!(cache.retain_restored(owner));
        }
        assert_eq!(privates.len(), 4);
        for private in &privates {
            // Fake::map rejects duplicate page insertion, so any remap fails.
            assert_eq!(private.pages.lock().unwrap().len(), 2);
            assert_eq!(private.events.lock().unwrap().len(), 32 * 4);
        }
        drop(cache);
        assert!(
            privates
                .iter()
                .all(|private| Arc::strong_count(private) == 1)
        );
    }

    #[test]
    fn cache_rejects_recycled_iova_and_any_authority_change() {
        let base = base();
        let private = Arc::new(Fake::default());
        let mut cache = DomainCache::new();
        assert!(cache.retain_restored(
            ExecutionDomain::prepare(base.clone(), private.clone(), 1, &[range()], 4096).unwrap()
        ));
        for changed in [
            Range {
                access: 3,
                ..range()
            },
            Range {
                coherent: false,
                ..range()
            },
            Range {
                kernel: true,
                ..range()
            },
            Range {
                size: 128,
                ..range()
            }, // cannot reuse a mapping superset
            Range {
                iova: 0x9000,
                size: 128,
                ..range()
            },
        ] {
            let authority = Authority::resolve(base.as_ref(), &[changed], 4096, 1).unwrap();
            assert!(cache.take_matching(&authority).is_none());
        }
        let streams = Authority::resolve(base.as_ref(), &[range()], 4096, 2).unwrap();
        assert!(cache.take_matching(&streams).is_none());
        base.pages.lock().unwrap().get_mut(&0x1000).unwrap().0 = 0x50000;
        let recycled = Authority::resolve(base.as_ref(), &[range()], 4096, 1).unwrap();
        assert!(cache.take_matching(&recycled).is_none());
        base.pages.lock().unwrap().remove(&0x1000);
        assert!(Authority::resolve(base.as_ref(), &[range()], 4096, 1).is_err());
        // An idle stale mapping is never routed just because its IOVA matches.
        assert!(private.events.lock().unwrap().is_empty());
    }

    #[test]
    fn cache_keys_canonical_union_not_range_order() {
        let base = base();
        let write = Range {
            iova: 0x1800,
            size: 128,
            access: 2,
            ..range()
        };
        let mut cache = DomainCache::new();
        let owner = ExecutionDomain::prepare(
            base.clone(),
            Arc::new(Fake::default()),
            1,
            &[range(), write],
            4096,
        )
        .unwrap();
        assert!(cache.retain_restored(owner));
        let reordered =
            Authority::resolve(base.as_ref(), &[write, range(), write], 4096, 1).unwrap();
        let owner = cache.take_matching(&reordered).unwrap();
        assert!(cache.retain_restored(owner));
        let read_only = Authority::resolve(base.as_ref(), &[range()], 4096, 1).unwrap();
        assert!(cache.take_matching(&read_only).is_none());
    }

    #[test]
    fn cache_bound_and_pressure_evict_only_idle_lru() {
        let base = base();
        let mut cache = DomainCache::new();
        let mut privates = Vec::new();
        for index in 0..4 {
            let private = Arc::new(Fake::default());
            let owner = ExecutionDomain::prepare(
                base.clone(),
                private.clone(),
                index + 1,
                &[range()],
                4096,
            )
            .unwrap();
            privates.push(private);
            assert!(cache.retain_restored(owner));
        }
        let authority = Authority::resolve(base.as_ref(), &[range()], 4096, 1).unwrap();
        let owner = cache.take_matching(&authority).unwrap();
        assert!(cache.retain_restored(owner)); // first entry is now newest
        cache.make_room();
        assert_eq!(Arc::strong_count(&privates[1]), 1);
        assert_eq!(Arc::strong_count(&privates[0]), 2);
        for remaining in [2, 3, 0] {
            assert!(cache.evict_oldest()); // allocation-pressure fallback
            assert_eq!(Arc::strong_count(&privates[remaining]), 1);
        }
        assert!(!cache.evict_oldest());
    }

    #[test]
    fn cache_never_reuses_partial_restore_or_enter_failure() {
        for entering in [true, false] {
            let base = base();
            let private = Arc::new(Fake {
                fail_route: Some((entering, 1)),
                ..Fake::default()
            });
            let mut owner =
                ExecutionDomain::prepare(base.clone(), private.clone(), 2, &[range()], 4096)
                    .unwrap();
            let stopped = halt_for_switch(|_| true).unwrap();
            if entering {
                assert!(owner.enter_halted(&stopped).is_err());
            } else {
                owner.enter_halted(&stopped).unwrap();
                assert!(owner.restore_halted(&stopped).is_err());
            }
            let mut cache = DomainCache::new();
            assert!(!cache.retain_restored(owner));
            let authority = Authority::resolve(base.as_ref(), &[range()], 4096, 2).unwrap();
            assert!(cache.take_matching(&authority).is_none());
            assert!(!cache.evict_oldest());
            assert_eq!(Arc::strong_count(&private), 2); // quarantined, never freed
        }
    }
    #[test]
    fn each_failed_halt_phase_prevents_any_route_change() {
        let phases = [
            HaltPhase::Idle,
            HaltPhase::Gx,
            HaltPhase::Clients,
            HaltPhase::Axi,
        ];
        for failure in 0..phases.len() {
            let mut seen = Vec::new();
            let base = base();
            let private = Arc::new(Fake::default());
            let mut owner =
                ExecutionDomain::prepare(base.clone(), private.clone(), 2, &[range()], 4096)
                    .unwrap();
            let stopped = halt_for_switch(|phase| {
                seen.push(phase);
                phase != phases[failure]
            });
            assert!(stopped.is_err());
            if let Ok(proof) = stopped {
                owner.enter_halted(&proof).unwrap();
            }
            assert_eq!(seen, phases[..=failure]);
            assert!(base.events.lock().unwrap().is_empty());
            assert!(private.events.lock().unwrap().is_empty());
            drop(owner);
            assert_eq!(Arc::strong_count(&private), 1);
        }
        let mut seen = Vec::new();
        assert!(
            halt_for_switch(|phase| {
                seen.push(phase);
                true
            })
            .is_ok()
        );
        assert_eq!(seen, phases);
    }

    #[test]
    fn maps_only_authorized_pages_at_identical_iovas() {
        let base = base();
        let private = Arc::new(Fake::default());
        let mut owner =
            ExecutionDomain::prepare(base.clone(), private.clone(), 2, &[range()], 4096).unwrap();
        assert_eq!(
            *private.pages.lock().unwrap(),
            BTreeMap::from([(0x1000, (0x10000, 1, true)), (0x2000, (0x30000, 1, true))])
        );
        owner
            .enter_halted(&halt_for_switch(|_| true).unwrap())
            .unwrap();
        owner
            .restore_halted(&halt_for_switch(|_| true).unwrap())
            .unwrap();
        assert_eq!(
            *base.events.lock().unwrap(),
            vec![(false, 0), (false, 1), (true, 0), (true, 1)]
        );
        assert_eq!(
            *private.events.lock().unwrap(),
            vec![(true, 0), (true, 1), (false, 0), (false, 1)]
        );
        drop(owner);
        assert_eq!(Arc::strong_count(&private), 1);
    }
    #[test]
    fn mapping_failures_never_change_routes() {
        for bad in [
            Range {
                iova: 0x3000,
                ..range()
            },
            Range {
                iova: u64::MAX,
                ..range()
            },
            Range { size: 0, ..range() },
        ] {
            let base = base();
            let private = Arc::new(Fake::default());
            assert!(
                ExecutionDomain::prepare(base.clone(), private.clone(), 1, &[bad], 4096).is_err()
            );
            assert!(base.events.lock().unwrap().is_empty());
            assert_eq!(Arc::strong_count(&private), 1);
        }
        let base = base();
        let private = Arc::new(Fake {
            fail_map: true,
            ..Fake::default()
        });
        assert!(ExecutionDomain::prepare(base.clone(), private, 1, &[range()], 4096).is_err());
        assert!(base.events.lock().unwrap().is_empty());
    }
    #[test]
    fn caller_data_aliases_union_without_importing_other_pages() {
        let private = Arc::new(Fake::default());
        let _owner = ExecutionDomain::prepare(
            base(),
            private.clone(),
            1,
            &[
                range(),
                Range {
                    iova: 0x1800,
                    size: 128,
                    access: 3,
                    ..range()
                },
            ],
            4096,
        )
        .unwrap();
        assert_eq!(
            private.pages.lock().unwrap().get(&0x1000),
            Some(&(0x10000, 3, true))
        );
        assert_eq!(
            private.pages.lock().unwrap().get(&0x2000),
            Some(&(0x30000, 1, true))
        );
        assert!(!private.pages.lock().unwrap().contains_key(&0x9000));
    }
    #[test]
    fn kernel_read_only_pages_cannot_be_upgraded_by_aliases() {
        for alias in [
            Range {
                access: 3,
                ..range()
            },
            Range {
                access: 3,
                kernel: true,
                ..range()
            },
            range(),
        ] {
            assert!(
                ExecutionDomain::prepare(
                    base(),
                    Arc::new(Fake::default()),
                    1,
                    &[
                        Range {
                            kernel: true,
                            ..range()
                        },
                        alias
                    ],
                    4096
                )
                .is_err()
            );
        }
    }
    #[test]
    fn diagnostic_domain_follows_route_and_does_not_restore_it() {
        let base = base();
        let fault = DomainFault {
            context_fault_pending: true,
            global_status: 1,
            context_status: 2,
            fault_address: 0x1234,
            syndrome0: 3,
            syndrome1: 4,
        };
        let private = Arc::new(Fake::default());
        let mut owner = ExecutionDomain::prepare(base.clone(), private.clone(), 1, &[range()], 4096).unwrap();
        let stopped = halt_for_switch(|_| true).unwrap();
        let page_iova = range().iova & !4095;
        let original = owner.diagnostic_domain().translate(page_iova);
        assert!(original.is_some());
        assert_eq!(owner.diagnostic_domain().fault_snapshot(), None);
        // Distinguish the two domain tables: a base lookup would conceal this.
        private.pages.lock().unwrap().remove(&page_iova);
        owner.enter_halted(&stopped).unwrap();
        *private.fault.lock().unwrap() = Some(fault);
        assert_eq!(owner.diagnostic_domain().translate(page_iova), None);
        assert_eq!(owner.diagnostic_domain().fault_snapshot(), Some(fault));
        assert!(owner.routed);
        owner.restore_halted(&stopped).unwrap();
        assert_eq!(owner.diagnostic_domain().translate(page_iova), original);
        assert_eq!(owner.diagnostic_domain().fault_snapshot(), None);
    }

    #[test]
    fn cached_fault_rejects_rebind_before_any_route_mutation() {
        for status in [0x402, 0x40000402] {
            let base = base();
            let private = Arc::new(Fake::default());
            let mut owner = ExecutionDomain::prepare(base.clone(), private.clone(), 1, &[range()], 4096).unwrap();
            let stopped = halt_for_switch(|_| true).unwrap();
            owner.enter_halted(&stopped).unwrap();
            owner.restore_halted(&stopped).unwrap();
            let mut cache = DomainCache::new();
            assert!(cache.retain_restored(owner));
            let original = DomainFault {
                context_fault_pending: true, global_status: 0, context_status: status,
                fault_address: 0x1115b3000, syndrome0: 3, syndrome1: 0x14000003,
            };
            *private.fault.lock().unwrap() = Some(original);
            let authority = Authority::resolve(base.as_ref(), &[range()], 4096, 1).unwrap();
            let mut cached = cache.take_matching(&authority).unwrap();
            let events = (base.events.lock().unwrap().clone(), private.events.lock().unwrap().clone());
            assert_eq!(cached.pending_bind_fault(), Some((true, Some(original))));
            assert_eq!(cached.enter_halted(&stopped), Err("a618 isolation: pending IOMMU fault before bind"));
            assert!(!cached.routed);
            assert_eq!(*base.events.lock().unwrap(), events.0);
            assert_eq!(*private.events.lock().unwrap(), events.1);
            assert_eq!(private.fault_snapshot(), Some(original));
        }
    }

    #[test]
    fn base_fault_is_attributed_to_base_but_format_only_can_bind() {
        let base = base();
        let private = Arc::new(Fake::default());
        let mut owner = ExecutionDomain::prepare(base.clone(), private.clone(), 1, &[range()], 4096).unwrap();
        let mut fault = DomainFault {
            context_fault_pending: true, global_status: 0, context_status: 0x402,
            fault_address: 0x1234, syndrome0: 2, syndrome1: 0,
        };
        *base.fault.lock().unwrap() = Some(fault);
        assert_eq!(owner.pending_bind_fault(), Some((false, Some(fault))));
        assert!(owner.enter_halted(&halt_for_switch(|_| true).unwrap()).is_err());
        assert!(base.events.lock().unwrap().is_empty());
        assert!(private.events.lock().unwrap().is_empty());
        fault.context_status = 0x400;
        fault.context_fault_pending = false;
        *base.fault.lock().unwrap() = Some(fault);
        *private.fault.lock().unwrap() = Some(fault);
        let stopped = halt_for_switch(|_| true).unwrap();
        owner.enter_halted(&stopped).unwrap();
        owner.restore_halted(&stopped).unwrap();
    }

    #[test]
    fn every_partial_route_failure_quarantines_owner() {
        for entering in [true, false] {
            for on_base in [true, false] {
                for stream in 0..2 {
                    let base = Arc::new(Fake {
                        fail_route: on_base.then_some((!entering, stream)),
                        ..Fake::default()
                    });
                    let private = Arc::new(Fake {
                        fail_route: (!on_base).then_some((entering, stream)),
                        ..Fake::default()
                    });
                    let mut owner =
                        ExecutionDomain::prepare(base.clone(), private.clone(), 2, &[], 4096)
                            .unwrap();
                    if entering {
                        assert!(
                            owner
                                .enter_halted(&halt_for_switch(|_| true).unwrap())
                                .is_err()
                        );
                    } else {
                        owner
                            .enter_halted(&halt_for_switch(|_| true).unwrap())
                            .unwrap();
                        assert!(
                            owner
                                .restore_halted(&halt_for_switch(|_| true).unwrap())
                                .is_err()
                        );
                    }
                    drop(owner);
                    assert_eq!(Arc::strong_count(&private), 2);
                    assert_eq!(Arc::strong_count(&base), 2);
                }
            }
        }
    }
}

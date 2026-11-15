//! Process-local reuse of immutable CPU compiler output, independent of GPU
//! contexts, module generations, code allocations and scratch allocations.
//!
//! A618 / native ABI 1 / COMPILER_CACHE_VERSION / MESA_REVISION are fixed for
//! the lifetime of this loaded backend. There is no disk persistence or sharing
//! with another GPU/compiler implementation. A semantic/compiler update loads
//! a different instance; none of these entries can survive that transition.
use crate::ir;
use alloc::{string::String, sync::Arc, vec::Vec};
use core::mem::{size_of, size_of_val};
use sgfx_shader_ir3::{BindingAssignment, BindingMap, CompiledShader, Error, Source, Stage};
use std::sync::Mutex;

const ENTRY_LIMIT: usize = 64;
const BYTE_LIMIT: usize = 16 * 1024 * 1024;
const SOURCE_LIMIT: usize = 256 * 1024;
const ENTRY_NAME_LIMIT: usize = 1024;
static CACHE: Mutex<Cache> = Mutex::new(Cache::new(ENTRY_LIMIT, BYTE_LIMIT));

struct Key {
    source: ir::ShaderSource,
    stage: Stage,
    entry: String,
    bindings: BindingMap,
}
impl Key {
    fn matches(
        &self,
        source: &ir::ShaderSource,
        stage: Stage,
        entry: &str,
        bindings: &BindingMap,
    ) -> bool {
        // Cheap discriminators first. No hash-only identity: language and every
        // source byte/decoded SPIR-V word participate in final equality.
        self.stage == stage
            && self.entry == entry
            && self.bindings == *bindings
            && self.source == *source
    }
    fn new(
        source: &ir::ShaderSource,
        stage: Stage,
        entry: &str,
        bindings: &BindingMap,
    ) -> Option<Self> {
        let source = match source {
            ir::ShaderSource::Wgsl(s) => {
                let mut owned = String::new();
                owned.try_reserve_exact(s.len()).ok()?;
                owned.push_str(s);
                ir::ShaderSource::Wgsl(owned)
            }
            ir::ShaderSource::SpirV(s) => {
                let mut owned = Vec::new();
                owned.try_reserve_exact(s.len()).ok()?;
                owned.extend_from_slice(s);
                ir::ShaderSource::SpirV(owned)
            }
        };
        let mut name = String::new();
        name.try_reserve_exact(entry.len()).ok()?;
        name.push_str(entry);
        // BindingMap contains at most 64 compact assignments; clone preserves
        // its validated canonical order and unused-stage bindings.
        Some(Self {
            source,
            stage,
            entry: name,
            bindings: bindings.clone(),
        })
    }
    fn heap_bytes(&self) -> usize {
        let source = match &self.source {
            ir::ShaderSource::Wgsl(s) => s.capacity(),
            ir::ShaderSource::SpirV(s) => vec_bytes(s),
        };
        source
            .saturating_add(self.entry.capacity())
            .saturating_add(self.bindings.assignments().len() * size_of::<BindingAssignment>())
    }
}
struct Entry {
    key: Key,
    artifact: Arc<CompiledShader>,
    bytes: usize,
}
struct Cache {
    entries: Vec<Entry>,
    bytes: usize,
    entry_limit: usize,
    byte_limit: usize,
}
impl Cache {
    const fn new(entry_limit: usize, byte_limit: usize) -> Self {
        Self {
            entries: Vec::new(),
            bytes: 0,
            entry_limit,
            byte_limit,
        }
    }
    fn get(
        &mut self,
        source: &ir::ShaderSource,
        stage: Stage,
        entry: &str,
        bindings: &BindingMap,
    ) -> Option<Arc<CompiledShader>> {
        let i = self
            .entries
            .iter()
            .position(|e| e.key.matches(source, stage, entry, bindings))?;
        let hit = self.entries.remove(i);
        let artifact = Arc::clone(&hit.artifact);
        self.entries.push(hit); // bounded LRU, no allocation on hit
        Some(artifact)
    }
    fn insert(&mut self, key: Key, artifact: Arc<CompiledShader>, artifact_bytes: usize) {
        let bytes = key.heap_bytes().saturating_add(artifact_bytes);
        // Charge the complete reserved Entry vector as well as entry heaps,
        // including unused slots. Oversized artifacts never evict useful ones.
        let overhead = self.entry_limit.saturating_mul(size_of::<Entry>());
        if self.entry_limit == 0 || bytes.saturating_add(overhead) > self.byte_limit {
            return;
        }
        if self.entries.capacity() < self.entry_limit {
            if self
                .entries
                .try_reserve_exact(self.entry_limit - self.entries.len())
                .is_err()
            {
                return;
            }
        }
        let overhead = self.entries.capacity().saturating_mul(size_of::<Entry>());
        if bytes.saturating_add(overhead) > self.byte_limit {
            return;
        }
        while self.entries.len() >= self.entry_limit
            || self.bytes.saturating_add(bytes).saturating_add(overhead) > self.byte_limit
        {
            let removed = self.entries.remove(0);
            self.bytes -= removed.bytes;
        }
        self.entries.push(Entry {
            key,
            artifact,
            bytes,
        });
        self.bytes += bytes;
    }
}
fn vec_bytes<T>(v: &Vec<T>) -> usize {
    v.capacity().saturating_mul(size_of::<T>())
}
fn eligible(source: &ir::ShaderSource, entry: &str) -> bool {
    let bytes = match source {
        ir::ShaderSource::Wgsl(s) => s.len(),
        ir::ShaderSource::SpirV(w) => w.len().saturating_mul(4),
    };
    bytes <= SOURCE_LIMIT && entry.len() <= ENTRY_NAME_LIMIT
}

/// Account all retained CPU buffers, not just the executable allocation.
/// Known vectors use actual capacities. Mesa's bounded, freshly deserialized
/// JSON extensions get a conservative 256 bytes/node (tree/vector overhead)
/// plus string capacity allowance. This exceeds serde_json's node storage and
/// doubling growth for this fixed compiler's <=1MiB metadata input. Saturation
/// makes any unrepresentable/oversized result uncacheable.
fn artifact_bytes(a: &CompiledShader) -> usize {
    let m = &a.metadata;
    let mut bytes = size_of::<CompiledShader>() + 2 * size_of::<usize>(); // Arc header
    for n in [
        a.entry.capacity(),
        vec_bytes(&a.allocation),
        vec_bytes(&a.bindings),
        vec_bytes(&a.spirv),
        m.stage.capacity(),
        vec_bytes(&m.imm_values),
        vec_bytes(&m.const_allocations),
        vec_bytes(&m.driver_ubos),
        vec_bytes(&m.ubo_ranges),
        vec_bytes(&m.inputs),
        vec_bytes(&m.outputs),
        vec_bytes(&m.sampler_prefetch),
        vec_bytes(&m.image_mapping.ssbo_to_tex),
        vec_bytes(&m.image_mapping.image_to_tex),
        vec_bytes(&m.image_mapping.tex_to_image),
    ] {
        bytes = bytes.saturating_add(n);
    }
    let mut pending: Vec<_> = m.extra.values().collect();
    for key in m.extra.keys() {
        bytes = bytes.saturating_add(key.capacity()).saturating_add(256);
    }
    while let Some(value) = pending.pop() {
        bytes = bytes.saturating_add(256.max(size_of_val(value)));
        if bytes > BYTE_LIMIT {
            return bytes;
        }
        if let Some(s) = value.as_str() {
            bytes = bytes.saturating_add(s.len().saturating_mul(2));
        }
        if let Some(array) = value.as_array() {
            pending.extend(array.iter());
        }
        if let Some(object) = value.as_object() {
            for (key, value) in object {
                bytes = bytes.saturating_add(key.capacity()).saturating_add(256);
                pending.push(value);
            }
        }
    }
    bytes
}

fn get_or_compile(
    cache: &Mutex<Cache>,
    source: &ir::ShaderSource,
    stage: Stage,
    entry: &str,
    bindings: &BindingMap,
    compile: impl FnOnce() -> Result<CompiledShader, Error>,
) -> Result<Arc<CompiledShader>, Error> {
    let cacheable = eligible(source, entry);
    if cacheable {
        if let Some(hit) = cache
            .lock()
            .ok()
            .and_then(|mut c| c.get(source, stage, entry, bindings))
        {
            return Ok(hit);
        }
    }
    // No cache lock across Naga/Mesa. Concurrent misses may compile twice; only
    // one successful result is retained. Failures are never cached.
    let artifact = Arc::new(compile()?);
    if !cacheable {
        return Ok(artifact);
    }
    let bytes = artifact_bytes(&artifact);
    if bytes > BYTE_LIMIT {
        return Ok(artifact);
    }
    let Some(key) = Key::new(source, stage, entry, bindings) else {
        return Ok(artifact);
    };
    if let Ok(mut c) = cache.lock() {
        if let Some(hit) = c.get(source, stage, entry, bindings) {
            return Ok(hit);
        }
        c.insert(key, Arc::clone(&artifact), bytes);
    }
    Ok(artifact)
}

/// Caller MUST resolve module ownership/generation before calling this function.
/// This returns only compiler output; allocate code and scratch in the caller's
/// context and keep its existing generation-aware ShaderRecord cache.
pub(crate) fn compile(
    source: &ir::ShaderSource,
    stage: Stage,
    entry: &str,
    bindings: &BindingMap,
) -> Result<Arc<CompiledShader>, Error> {
    get_or_compile(&CACHE, source, stage, entry, bindings, || {
        let source = match source {
            ir::ShaderSource::Wgsl(s) => Source::Wgsl(s),
            ir::ShaderSource::SpirV(s) => Source::SpirV(s),
        };
        sgfx_shader_ir3::compile_with_bindings(source, stage, entry, bindings)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;
    const SHADER: &str =
        "@fragment fn main()->@location(0) vec4<f32>{return vec4<f32>(0.25,0.5,0.75,1.0);}";
    fn artifact() -> CompiledShader {
        sgfx_shader_ir3::compile(Source::Wgsl(SHADER), Stage::Fragment, "main").unwrap()
    }
    fn map(slot: u32) -> BindingMap {
        BindingMap::new(alloc::vec![BindingAssignment {
            group: 0,
            binding: 3,
            kind: sgfx_shader_ir3::ResourceKind::UniformBuffer,
            slot
        }])
        .unwrap()
    }

    #[test]
    fn hit_reuses_cpu_arc_across_equivalent_sources_and_discriminates_complete_keys() {
        let cache = Mutex::new(Cache::new(64, BYTE_LIMIT));
        let a = artifact();
        let calls = Cell::new(0);
        let source = ir::ShaderSource::Wgsl(SHADER.into());
        let bindings = map(0);
        let run = |source: &ir::ShaderSource, stage, entry: &str, bindings: &BindingMap| {
            get_or_compile(&cache, source, stage, entry, bindings, || {
                assert!(cache.try_lock().is_ok());
                calls.set(calls.get() + 1);
                Ok(a.clone())
            })
            .unwrap()
        };
        let first = run(&source, Stage::Fragment, "main", &bindings);
        let second = run(
            &ir::ShaderSource::Wgsl(SHADER.into()),
            Stage::Fragment,
            "main",
            &map(0),
        );
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(calls.get(), 1);
        for (s, stage, entry, b) in [
            (
                ir::ShaderSource::Wgsl(alloc::format!("{SHADER}\n")),
                Stage::Fragment,
                "main",
                map(0),
            ),
            (source.clone(), Stage::Vertex, "main", map(0)),
            (source.clone(), Stage::Fragment, "other", map(0)),
            (source.clone(), Stage::Fragment, "main", map(1)),
            (
                ir::ShaderSource::SpirV(a.spirv.clone()),
                Stage::Fragment,
                "main",
                map(0),
            ),
        ] {
            assert!(!Arc::ptr_eq(&first, &run(&s, stage, entry, &b)));
        }
        assert_eq!(calls.get(), 6);
        // Retaining CPU bytes cannot authorize a retired SGFX module handle.
        let table = ir::ResourceTable::new();
        let module = table
            .define_shader_module(ir::ShaderModuleDesc::wgsl(SHADER.into()).unwrap())
            .unwrap();
        let id = module.id();
        table.release_shader_module(id).unwrap();
        assert!(table.shader_module_ref(id).is_err());
        assert!(Arc::ptr_eq(
            &first,
            &run(&source, Stage::Fragment, "main", &bindings)
        ));
    }
    #[test]
    fn failures_and_excessive_sources_are_not_saved() {
        let cache = Mutex::new(Cache::new(64, BYTE_LIMIT));
        let bindings = map(0);
        let calls = Cell::new(0);
        for _ in 0..2 {
            assert!(
                get_or_compile(
                    &cache,
                    &ir::ShaderSource::Wgsl("bad".into()),
                    Stage::Fragment,
                    "main",
                    &bindings,
                    || {
                        calls.set(calls.get() + 1);
                        Err(Error("invalid".into()))
                    }
                )
                .is_err()
            );
        }
        assert_eq!(calls.get(), 2);
        assert!(cache.lock().unwrap().entries.is_empty());
        let a = artifact();
        let source = ir::ShaderSource::Wgsl(" ".repeat(SOURCE_LIMIT + 1));
        for _ in 0..2 {
            get_or_compile(&cache, &source, Stage::Fragment, "main", &bindings, || {
                calls.set(calls.get() + 1);
                Ok(a.clone())
            })
            .unwrap();
        }
        assert_eq!(calls.get(), 4);
        assert!(cache.lock().unwrap().entries.is_empty());
    }
    #[test]
    fn entry_and_total_byte_limits_evict_lru_without_revoking_live_arcs() {
        let a = artifact();
        let source = ir::ShaderSource::Wgsl(SHADER.into());
        let bindings = map(0);
        let cache = Mutex::new(Cache::new(2, BYTE_LIMIT));
        let get = |entry| {
            get_or_compile(&cache, &source, Stage::Fragment, entry, &bindings, || {
                Ok(a.clone())
            })
            .unwrap()
        };
        let first = get("a");
        let second = get("b");
        assert!(Arc::ptr_eq(&first, &get("a")));
        get("c");
        assert_eq!(cache.lock().unwrap().entries.len(), 2);
        assert!(!Arc::ptr_eq(&second, &get("b")));
        assert_eq!(first.allocation, a.allocation);
        let cost = artifact_bytes(&a)
            + Key::new(&source, Stage::Fragment, "a", &bindings)
                .unwrap()
                .heap_bytes();
        let cache = Mutex::new(Cache::new(4, 4 * size_of::<Entry>() + cost));
        for entry in ["a", "b", "c"] {
            get_or_compile(&cache, &source, Stage::Fragment, entry, &bindings, || {
                Ok(a.clone())
            })
            .unwrap();
            let c = cache.lock().unwrap();
            assert_eq!(c.entries.len(), 1);
            assert!(c.bytes + c.entries.capacity() * size_of::<Entry>() <= c.byte_limit);
        }
        let mut huge = a.clone();
        huge.allocation = alloc::vec![0;cost+4096];
        get_or_compile(&cache, &source, Stage::Fragment, "huge", &bindings, || {
            Ok(huge)
        })
        .unwrap();
        assert_eq!(cache.lock().unwrap().entries[0].key.entry, "c");
    }
}

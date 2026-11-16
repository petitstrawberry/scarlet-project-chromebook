// SPDX-License-Identifier: GPL-2.0-only
//! 20 MHz STA channels within the ath world-SKU bands. Discovery is passive,
//! including DFS; joining is only permitted after receiving an AP beacon.
pub const FIVE_GHZ: [u8; 25] = [
    36, 40, 44, 48, 52, 56, 60, 64, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144,
    149, 153, 157, 161, 165,
];
pub fn supported(channel: u8) -> bool {
    (1..=13).contains(&channel) || FIVE_GHZ.contains(&channel)
}
pub fn frequency(channel: u8) -> Option<u32> {
    if (1..=13).contains(&channel) {
        Some(2407 + u32::from(channel) * 5)
    } else if FIVE_GHZ.contains(&channel) {
        Some(5000 + u32::from(channel) * 5)
    } else {
        None
    }
}
pub fn channels() -> impl Iterator<Item = u8> {
    (1..=13).chain(FIVE_GHZ)
}
pub fn dfs(channel: u8) -> bool {
    (52..=64).contains(&channel) || (100..=144).contains(&channel)
}
pub fn phy(channel: u8) -> u32 {
    u32::from(channel <= 14)
}
pub fn rate_supported(channel: u8, rate: u8) -> bool {
    const OFDM: [u8; 8] = [12, 18, 24, 36, 48, 72, 96, 108];
    OFDM.contains(&(rate & 0x7f)) || (channel <= 14 && [2, 4, 11, 22].contains(&(rate & 0x7f)))
}
pub fn management_rate(channel: u8, rates: &[u8]) -> Option<u32> {
    let rate = rates
        .iter()
        .filter(|v| **v & 0x80 != 0 && rate_supported(channel, **v))
        .map(|v| v & 0x7f)
        .min()
        .or_else(|| {
            rates
                .iter()
                .filter(|v| rate_supported(channel, **v))
                .map(|v| v & 0x7f)
                .min()
        })?;
    if let Some(i) = [22, 11, 4, 2].iter().position(|v| *v == rate) {
        Some(0x40 + i as u32)
    } else {
        [96, 48, 24, 12, 108, 72, 36, 18]
            .iter()
            .position(|v| *v == rate)
            .map(|i| i as u32)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separates_bands_and_rejects_nonchannels() {
        assert_eq!(frequency(13), Some(2472));
        assert_eq!(frequency(36), Some(5180));
        assert_eq!(frequency(100), Some(5500));
        assert_eq!(frequency(15), None);
        assert!(!rate_supported(36, 0x82));
        assert_eq!(management_rate(36, &[0x8c, 0x98, 0xb0]), Some(3));
        assert_eq!(management_rate(1, &[0x82, 0x84]), Some(0x43));
    }
}

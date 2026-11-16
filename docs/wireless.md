# CoachZ onboard Wi-Fi

The SC7180/WCN3990 driver provides an infrastructure STA through Scarlet's
native wireless API. `wirelessd` runs the common WPA2 supplicant and `netcfgd`
manages DHCP/DNS. Settings and Control Center use the same service as `wifictl`.

Physical CoachZ validation has covered passive discovery in both bands,
WPA2-Personal/CCMP association on 2.4 GHz and 5 GHz channel 36, DHCP, DNS,
bidirectional ICMP, and 1 MiB SFTP upload/download with identical contents.
Disconnect/reconnect on the same boot and simultaneous USB networking have
also passed. These checks use a dedicated Raspberry Pi AP. A household 5 GHz
AP has also completed WPA2, DHCP and a matching 1 MiB SFTP round trip. Its
RSNXE must be preserved alongside the beacon RSNE so the common supplicant can
validate message 3 against the AP's advertised security information.

The initial PHY is legacy 20 MHz (802.11a/g/b rates). HT/VHT aggregation, WPA3,
PMF-required networks, AP mode, suspend/resume and automatic firmware recovery
are not advertised. GTK rotation is not advertised yet. A firmware fault closes
the link and quarantines DMA; it requires a reboot. This is initial STA support,
not a claim of complete ath10k feature parity.

## Firmware

Before building an image, run:

```sh
python3 scripts/fetch-wifi-firmware.py
```

`projects/aarch64-coachz-limine/wifi-firmware.json` pins source revisions,
archive SHA-512, file SHA-256 and sizes. The fetcher imports only the non-LTE
MSS pair and WCN3990 board/feature inputs. Existing differing files are rejected.
Binaries remain in ignored `firmware/wifi/`; they are not redistributed in Git.
`--verify` checks local inputs offline; `--archive FILE` reuses the pinned MSS
archive. Build-time installation verifies inputs and never downloads implicitly.
Both initramfs and rootfs install the same firmware bundle.

`firmware-5.bin` contains ath10k feature metadata, not the WLAN executable.
The signed MSS image supplies that executable. The chip reports ID `0x320`,
family `0x4001`, board `0x67`; the selected board-data record is 26,328 bytes.
The feature metadata does not advertise raw RX, so the driver configures Native
Wi-Fi decapsulation and uses hardware CCMP.

## Driver path

1. SMEM v12 validates the partition table and allocates items under the TCSR
   hardware lock. SCM SMCCC64 transfers HLOS/MSS memory ownership. MSS clocks,
   resets, signed MBA/MPSS authentication and SMP2P establish remoteproc startup.
2. IRQ-driven GLINK/IPCRTR carries QRTR and WLFW QMI. RMTFS provides the
   non-LTE modem's shared scratch carveout. Board data, MSA, regulator votes and
   mission-mode CE configuration complete before radio publication.
3. SNOC Copy Engines carry HTC, WMI and HTT 3.x with translated WLAN and
   firmware SMMU streams. GICv3 honors the DT edge trigger needed by GLINK.
   Bounded queues and one-shot deadlines drive work without idle polling.
4. Host MLME parses beacons, authenticates, associates and handles management
   completions. Both bands use passive discovery and the EEPROM world SKU;
   joining requires an observed AP. The driver does not force a country code
   or initiate an AP. The initial 5 GHz association uses 20 MHz channels.
5. The common controlled port gates ordinary Ethernet traffic. Hardware key
   completion is confirmed by HTT security indications. Firmware peer
   authorization follows PTK commitment. Initial EAPOL message 4 remains
   host-owned until that barrier, avoiding a TX-drain/key-install dependency
   cycle. Disconnect notifies the AP and waits for both data and management
   DMA completions before peer/vdev retirement.

CE source completion releases only the copied command. HTT completion releases
TX payload/fragment leases; management TX has its own completion. WCN3990's
command-copy reuse guard also scrubs retired key commands. On uncertain firmware
ownership the driver retains DMA allocations and disables the radio rather than
reusing them. All copied key material is erased after its required lifetime.

The non-LTE RMTFS backend starts with four blank 2 MiB images, matching
msm-cros-efs-loader's Wi-Fi-only setup. Paths and every sector/physical range
are validated before I/O. Writes survive only within the modem lifetime; the
driver does not import cellular NV/FSG data or claim durable persistence.
Firmware for asynchronous callbacks is captured during probe, before userspace
root changes can affect path lookup.

A firmware MAC is used when valid. Otherwise trusted entropy supplies a local
unicast address retained for the boot. The USB network path remains independent.
Scarlet's common ABI and supplicant contracts are documented in its
`docs/network/wireless.md`. The Switch BCM4356 implementation informs common
queue admission and security retirement; its fullmac MLME is not used by this
host-MLME radio.

## Runtime

Use Settings, or:

```sh
wifictl scan
wifictl connect YOUR_SSID --password-file /path/to/private/passphrase
wifictl status
wifictl disconnect
```

The password file contains the passphrase; avoid putting it in a shell command
or sharing it in logs. A profile is saved only after successful authentication.
Disconnect before scanning when the service reports scanning unavailable.
USB access can be retained while bringing up the wireless interface.

Protocol and startup references: [CoachZ DT][coachz], [SC7180 DT][sc7180],
[Trogdor DT][trogdor], Linux [ath10k SNOC][snoc] and [WLFW QMI][qmi].

[coachz]: https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/arch/arm64/boot/dts/qcom/sc7180-trogdor-coachz.dtsi
[sc7180]: https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/arch/arm64/boot/dts/qcom/sc7180.dtsi
[trogdor]: https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/arch/arm64/boot/dts/qcom/sc7180-trogdor.dtsi
[snoc]: https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/net/wireless/ath/ath10k/snoc.c
[qmi]: https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/net/wireless/ath/ath10k/qmi.c

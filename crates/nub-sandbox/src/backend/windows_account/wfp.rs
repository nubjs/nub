//! Windows Filtering Platform egress fence for the dedicated sandbox account.
//!
//! THE MODEL (mirrors SRT's `vendor/srt-win-src/src/wfp.rs`, read 2026-07-24): four
//! PERSISTENT filters in one nub-owned persistent sublayer key the deny on
//! `FWPM_CONDITION_ALE_USER_ID` — the connecting token's user — so **every** process the
//! sandboxed child spawns is fenced by the same rule. That is what defeats surrogate-spawn:
//! a process-keyed filter loses the moment the child launches a helper, a user-keyed one
//! cannot.
//!
//! WHY A PORT RANGE, NOT THE EXACT PROXY PORT (the decision that keeps every run
//! UNELEVATED): every `Fwpm*Add`/`Delete` needs administrator, so a per-run filter carrying
//! the run's ephemeral proxy port would mean a UAC prompt per run. Instead the one-time
//! elevated install pre-authorizes a narrow LOOPBACK PORT WINDOW, and the proxy binds
//! *into* that window at launch (`proxy::EgressProxy::start_in_range`). One elevated
//! install ever; every run after it touches WFP not at all. The honest cost is that the
//! permit is a ~10-port loopback window rather than one exact port, and it is not
//! user-scoped — any local principal may connect to loopback inside it. Codex takes the
//! other branch (exact ports baked into the rule) and pays a fresh UAC prompt whenever the
//! port changes.
//!
//! ARBITRATION: WFP picks the highest-weight matching filter within a sublayer, so the
//! loopback PERMIT must out-weigh the account BLOCK. That ordering is the entire fence and
//! is const-asserted below. Weights stay under 2^60 so they land in WFP's manual-weight
//! class (the top nibble is the auto-classifier's).
//!
//! HONEST GAPS (both references share them; see LIMITATIONS.md):
//!   - Inbound (`ALE_AUTH_RECV_ACCEPT_*`) and bind (`ALE_RESOURCE_ASSIGNMENT_*`) are NOT
//!     filtered — the account may still listen. Egress is the axis nub's grammar names.
//!   - DNS still resolves: `getaddrinfo` is serviced by the `Dnscache` service running as
//!     `NETWORK SERVICE`, so the lookup succeeds under a different token even though the
//!     subsequent `connect()` is blocked. Inherent to keying on the connecting token; no
//!     filter set can close it. Harmless here — the child talks to nub's proxy by IP and
//!     the PROXY resolves.

#![cfg(target_os = "windows")]

use std::io;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::*;
use windows_sys::core::GUID;

/// nub's own WFP provider — every object this module installs carries it, so the uninstall
/// sweep can enumerate exactly nub's filters and nothing else. Stable forever: regenerating
/// it would orphan every filter written by an older nub.
const NUB_PROVIDER_KEY: GUID = GUID::from_u128(0x6f1c9e42_5a3d_4b17_9d84_2c7e0f5a8b31);
const NUB_SUBLAYER_KEY: GUID = GUID::from_u128(0x9a4b7d13_0e62_4c58_bf29_71d3a6e4c085);

/// Sublayer weight. Mirrors SRT/Codex; high enough to sit above the default filtering
/// sublayers, low enough not to fight IPsec.
const SUBLAYER_WEIGHT: u16 = 0x8000;

/// The loopback PERMIT must out-weigh the account BLOCK — that ordering IS the fence.
const W_LOOPBACK_PERMIT: u64 = 0x0F80_0000_0000_0000;
const W_ACCOUNT_BLOCK: u64 = 0x0F40_0000_0000_0000;
const _: () = assert!(W_LOOPBACK_PERMIT > W_ACCOUNT_BLOCK);

/// The loopback window the elevated install pre-authorizes, which the egress proxy then
/// binds into. Ten ports leaves headroom for two concurrent listeners plus bind retries.
pub(crate) const DEFAULT_PROXY_PORT_RANGE: (u16, u16) = (59080, 59089);

/// A wider window would stop meaningfully narrowing loopback, so the install refuses one.
pub(crate) const MAX_PROXY_PORT_RANGE_WIDTH: u16 = 64;

// The default window must itself satisfy the rule the installer enforces on a user-supplied
// one, or the out-of-the-box setup would be refused by its own validation.
const _: () = {
    let (low, high) = DEFAULT_PROXY_PORT_RANGE;
    assert!(low > 0 && high >= low);
    assert!(high - low < MAX_PROXY_PORT_RANGE_WIDTH);
};

/// BFE reports access-denied as EITHER of these — test both or an unelevated caller looks
/// like a hard failure.
const FWP_E_ACCESS_DENIED: u32 = 0x8032_0028;
const ERROR_ACCESS_DENIED: u32 = 5;
const FWP_E_ALREADY_EXISTS: u32 = 0x8032_0009;
const FWP_E_FILTER_NOT_FOUND: u32 = 0x8032_0003;
const FWP_E_SUBLAYER_NOT_FOUND: u32 = 0x8032_0007;
const FWP_E_PROVIDER_NOT_FOUND: u32 = 0x8032_0006;
const FWP_E_IN_USE: u32 = 0x8032_000A;

/// `RPC_C_AUTHN_DEFAULT`, spelled locally so this module needs no `Win32_System_Rpc` import
/// beyond the one the binding's own signature forces.
const RPC_C_AUTHN_DEFAULT: u32 = 0xFFFF_FFFF;

/// Whether a raw WFP status means "you are not elevated" rather than a real fault.
pub(crate) fn is_access_denied(rc: u32) -> bool {
    rc == FWP_E_ACCESS_DENIED || rc == ERROR_ACCESS_DENIED
}

fn wfp_err(op: &'static str, rc: u32) -> io::Error {
    if is_access_denied(rc) {
        return io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{op} failed: administrator rights are required to change WFP filters"),
        );
    }
    io::Error::other(format!("{op} failed (WFP status {rc:#010x})"))
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An open BFE engine handle, closed on drop.
struct Engine(HANDLE);

impl Engine {
    fn open() -> io::Result<Self> {
        let mut h: HANDLE = std::ptr::null_mut();
        // SAFETY: all-null optional params; `h` is a valid out-slot.
        let rc = unsafe {
            FwpmEngineOpen0(
                std::ptr::null(),
                RPC_C_AUTHN_DEFAULT,
                std::ptr::null(),
                std::ptr::null(),
                &mut h,
            )
        };
        if rc != 0 {
            return Err(wfp_err("FwpmEngineOpen0", rc));
        }
        Ok(Engine(h))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful FwpmEngineOpen0 and is closed once.
        unsafe { FwpmEngineClose0(self.0) };
    }
}

/// Run `f` inside a WFP transaction, aborting on any error or unwind. The whole install is
/// one transaction so a mid-install failure cannot leave a half-fence — which would be
/// fail-OPEN (block filters missing, permit present).
fn in_transaction<T>(engine: &Engine, f: impl FnOnce(&Engine) -> io::Result<T>) -> io::Result<T> {
    // SAFETY: engine handle is live for the whole call.
    let rc = unsafe { FwpmTransactionBegin0(engine.0, 0) };
    if rc != 0 {
        return Err(wfp_err("FwpmTransactionBegin0", rc));
    }
    struct AbortOnDrop(HANDLE);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            unsafe { FwpmTransactionAbort0(self.0) };
        }
    }
    let abort = AbortOnDrop(engine.0);
    let out = f(engine)?;
    // SAFETY: still inside the transaction opened above.
    let rc = unsafe { FwpmTransactionCommit0(engine.0) };
    if rc != 0 {
        return Err(wfp_err("FwpmTransactionCommit0", rc));
    }
    std::mem::forget(abort); // disarm ONLY after a successful commit
    Ok(out)
}

/// A self-relative security descriptor built from SDDL, `LocalFree`d on drop. This is the
/// value the `ALE_USER_ID` condition carries: WFP runs `AccessCheck` on the connecting
/// token against it, so the filter matches exactly when the token is the sandbox account.
struct OwnedSd {
    ptr: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
    len: u32,
}

impl OwnedSd {
    /// `G:` (primary group) is REQUIRED — not by the kernel, but because user-mode
    /// `AccessCheck` rejects a group-less descriptor with `ERROR_INVALID_SECURITY_DESCR`,
    /// which is how the host-side unit test validates the shape. `LS` (Local Service) is
    /// just a stable always-present principal; its value never enters DACL evaluation.
    /// The `CC` right is an arbitrary single bit — WFP only cares whether the check grants.
    fn for_sid(sid: &str) -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        let sddl = to_wide(&format!("O:LSG:LSD:(A;;CC;;;{sid})"));
        let mut ptr = std::ptr::null_mut();
        let mut len: u32 = 0;
        // SAFETY: `sddl` is NUL-terminated UTF-16; both out-params are valid slots.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1, // SDDL_REVISION_1
                &mut ptr,
                &mut len,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedSd { ptr, len })
    }

    fn blob(&self) -> FWP_BYTE_BLOB {
        FWP_BYTE_BLOB {
            size: self.len,
            data: self.ptr.cast(),
        }
    }
}

impl Drop for OwnedSd {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from ConvertStringSecurityDescriptorToSecurityDescriptorW,
        // which documents LocalFree as the release.
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.ptr.cast()) };
    }
}

/// Backing storage every condition value points INTO. WFP's condition unions hold RAW
/// POINTERS, so these slots must outlive `FwpmFilterAdd0` — the single easiest way to write
/// a filter that silently reads freed memory.
struct ConditionSlots {
    v4: FWP_V4_ADDR_AND_MASK,
    v6: FWP_BYTE_ARRAY16,
    port_range: FWP_RANGE0,
    sd_blob: FWP_BYTE_BLOB,
    weight: u64,
}

/// The layer/action/conditions of one installed filter, kept as plain data so the four-way
/// filter table reads as a table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FilterSpec {
    /// Permit the account to reach nub's proxy: loopback, inside the pre-authorized window.
    LoopbackPermitV4,
    LoopbackPermitV6,
    /// Deny the account everything else.
    AccountBlockV4,
    AccountBlockV6,
}

impl FilterSpec {
    const ALL: [FilterSpec; 4] = [
        FilterSpec::LoopbackPermitV4,
        FilterSpec::LoopbackPermitV6,
        FilterSpec::AccountBlockV4,
        FilterSpec::AccountBlockV6,
    ];

    fn layer(self) -> GUID {
        match self {
            FilterSpec::LoopbackPermitV4 | FilterSpec::AccountBlockV4 => {
                FWPM_LAYER_ALE_AUTH_CONNECT_V4
            }
            FilterSpec::LoopbackPermitV6 | FilterSpec::AccountBlockV6 => {
                FWPM_LAYER_ALE_AUTH_CONNECT_V6
            }
        }
    }

    fn is_permit(self) -> bool {
        matches!(
            self,
            FilterSpec::LoopbackPermitV4 | FilterSpec::LoopbackPermitV6
        )
    }

    fn name(self) -> &'static str {
        match self {
            FilterSpec::LoopbackPermitV4 => "nub sandbox: permit loopback proxy (IPv4)",
            FilterSpec::LoopbackPermitV6 => "nub sandbox: permit loopback proxy (IPv6)",
            FilterSpec::AccountBlockV4 => "nub sandbox: block egress for sandbox account (IPv4)",
            FilterSpec::AccountBlockV6 => "nub sandbox: block egress for sandbox account (IPv6)",
        }
    }
}

/// Install the four-filter egress fence for `sid`, permitting only loopback TCP inside
/// `port_range`. ELEVATED. Idempotent: any previously installed nub filters are swept
/// inside the same transaction first, so re-running never accretes duplicates.
pub(crate) fn install(sid: &str, port_range: (u16, u16)) -> io::Result<()> {
    let (low, high) = port_range;
    if low == 0 || high < low {
        return Err(io::Error::other(format!(
            "invalid sandbox proxy port range {low}-{high}"
        )));
    }
    if high - low >= MAX_PROXY_PORT_RANGE_WIDTH {
        return Err(io::Error::other(format!(
            "sandbox proxy port range {low}-{high} is wider than the {MAX_PROXY_PORT_RANGE_WIDTH}-port maximum \
             — a wide window stops meaningfully narrowing loopback"
        )));
    }

    let sd = OwnedSd::for_sid(sid)?;
    let engine = Engine::open()?;

    in_transaction(&engine, |e| {
        // Sweeping inside the transaction is what makes re-install idempotent. A swallowed
        // enum error here would silently skip the sweep and grow the filter count on every
        // install, so it propagates.
        delete_nub_filters(e)?;
        add_provider(e)?;
        add_sublayer(e)?;

        let mut slots = ConditionSlots {
            v4: FWP_V4_ADDR_AND_MASK {
                addr: 0x7F00_0000, // 127.0.0.0
                mask: 0xFF00_0000, // /8
            },
            v6: {
                let mut a = FWP_BYTE_ARRAY16 {
                    byteArray16: [0; 16],
                };
                a.byteArray16[15] = 1; // ::1
                a
            },
            port_range: FWP_RANGE0 {
                valueLow: uint16_value(low),
                valueHigh: uint16_value(high),
            },
            sd_blob: sd.blob(),
            weight: 0,
        };

        for spec in FilterSpec::ALL {
            add_filter(e, spec, &mut slots)?;
        }
        Ok(())
    })
}

/// Remove every WFP object nub installed. ELEVATED. Tolerant of a partially-installed or
/// already-clean state so it can also serve as crash-residue recovery.
pub(crate) fn uninstall() -> io::Result<()> {
    let engine = Engine::open()?;
    in_transaction(&engine, |e| {
        delete_nub_filters(e)?;
        // SAFETY: engine handle live; key is a 'static GUID.
        let rc = unsafe { FwpmSubLayerDeleteByKey0(e.0, &NUB_SUBLAYER_KEY) };
        // FWP_E_IN_USE means a foreign filter still sits under our sublayer — leave it.
        if rc != 0 && rc != FWP_E_SUBLAYER_NOT_FOUND && rc != FWP_E_IN_USE {
            return Err(wfp_err("FwpmSubLayerDeleteByKey0", rc));
        }
        // SAFETY: as above.
        let rc = unsafe { FwpmProviderDeleteByKey0(e.0, &NUB_PROVIDER_KEY) };
        if rc != 0 && rc != FWP_E_PROVIDER_NOT_FOUND && rc != FWP_E_IN_USE {
            return Err(wfp_err("FwpmProviderDeleteByKey0", rc));
        }
        Ok(())
    })
}

/// How many nub filters are currently installed. ELEVATED — WFP gates even ENUMERATION on
/// administrator, which is why an unelevated `status` cannot use this and must fall back to
/// the behavioral probe instead.
pub(crate) fn installed_filter_count() -> io::Result<usize> {
    let engine = Engine::open()?;
    let mut n = 0usize;
    for layer in [
        FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        FWPM_LAYER_ALE_AUTH_CONNECT_V6,
    ] {
        n += enum_nub_filter_keys(&engine, layer)?.len();
    }
    Ok(n)
}

fn add_provider(e: &Engine) -> io::Result<()> {
    let mut name = to_wide("nub sandbox");
    let mut desc = to_wide("nub OS-enforced sandbox egress fence");
    let provider = FWPM_PROVIDER0 {
        providerKey: NUB_PROVIDER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: name.as_mut_ptr(),
            description: desc.as_mut_ptr(),
        },
        flags: FWPM_PROVIDER_FLAG_PERSISTENT,
        providerData: FWP_BYTE_BLOB {
            size: 0,
            data: std::ptr::null_mut(),
        },
        serviceName: std::ptr::null_mut(),
    };
    // SAFETY: `name`/`desc` outlive the call; provider is fully initialized.
    let rc = unsafe { FwpmProviderAdd0(e.0, &provider, std::ptr::null_mut()) };
    if rc != 0 && rc != FWP_E_ALREADY_EXISTS {
        return Err(wfp_err("FwpmProviderAdd0", rc));
    }
    Ok(())
}

fn add_sublayer(e: &Engine) -> io::Result<()> {
    let mut name = to_wide("nub sandbox");
    let mut desc = to_wide("nub sandbox account egress fence");
    let mut provider_key = NUB_PROVIDER_KEY;
    let sublayer = FWPM_SUBLAYER0 {
        subLayerKey: NUB_SUBLAYER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: name.as_mut_ptr(),
            description: desc.as_mut_ptr(),
        },
        flags: FWPM_SUBLAYER_FLAG_PERSISTENT,
        providerKey: &mut provider_key,
        providerData: FWP_BYTE_BLOB {
            size: 0,
            data: std::ptr::null_mut(),
        },
        weight: SUBLAYER_WEIGHT,
    };
    // SAFETY: every referenced buffer outlives the call.
    let rc = unsafe { FwpmSubLayerAdd0(e.0, &sublayer, std::ptr::null_mut()) };
    if rc != 0 && rc != FWP_E_ALREADY_EXISTS {
        return Err(wfp_err("FwpmSubLayerAdd0", rc));
    }
    Ok(())
}

fn uint16_value(v: u16) -> FWP_VALUE0 {
    FWP_VALUE0 {
        r#type: FWP_UINT16,
        Anonymous: FWP_VALUE0_0 { uint16: v },
    }
}

fn add_filter(e: &Engine, spec: FilterSpec, slots: &mut ConditionSlots) -> io::Result<()> {
    let mut conditions: Vec<FWPM_FILTER_CONDITION0> = Vec::with_capacity(2);

    if spec.is_permit() {
        let addr_value = match spec {
            FilterSpec::LoopbackPermitV4 => FWP_CONDITION_VALUE0 {
                r#type: FWP_V4_ADDR_MASK,
                Anonymous: FWP_CONDITION_VALUE0_0 {
                    v4AddrMask: &mut slots.v4,
                },
            },
            _ => FWP_CONDITION_VALUE0 {
                r#type: FWP_BYTE_ARRAY16_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 {
                    byteArray16: &mut slots.v6,
                },
            },
        };
        conditions.push(FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_IP_REMOTE_ADDRESS,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: addr_value,
        });
        conditions.push(FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
            matchType: FWP_MATCH_RANGE,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_RANGE_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 {
                    rangeValue: &mut slots.port_range,
                },
            },
        });
    } else {
        conditions.push(FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_ALE_USER_ID,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 {
                    sd: &mut slots.sd_blob,
                },
            },
        });
    }

    slots.weight = if spec.is_permit() {
        W_LOOPBACK_PERMIT
    } else {
        W_ACCOUNT_BLOCK
    };

    let mut name = to_wide(spec.name());
    let mut provider_key = NUB_PROVIDER_KEY;
    let filter = FWPM_FILTER0 {
        // A zero filterKey makes WFP mint one; identity comes from the provider key, which
        // is what the uninstall sweep enumerates on.
        displayData: FWPM_DISPLAY_DATA0 {
            name: name.as_mut_ptr(),
            description: std::ptr::null_mut(),
        },
        flags: FWPM_FILTER_FLAG_PERSISTENT,
        providerKey: &mut provider_key,
        layerKey: spec.layer(),
        subLayerKey: NUB_SUBLAYER_KEY,
        weight: FWP_VALUE0 {
            r#type: FWP_UINT64,
            Anonymous: FWP_VALUE0_0 {
                uint64: &mut slots.weight,
            },
        },
        numFilterConditions: conditions.len() as u32,
        filterCondition: conditions.as_mut_ptr(),
        action: FWPM_ACTION0 {
            r#type: if spec.is_permit() {
                FWP_ACTION_PERMIT
            } else {
                FWP_ACTION_BLOCK
            },
            Anonymous: FWPM_ACTION0_0 {
                filterType: GUID::from_u128(0),
            },
        },
        ..Default::default()
    };

    // SAFETY: `conditions`, `slots`, `name` and `provider_key` all outlive this call — WFP
    // stores raw pointers into every one of them for the duration of the add.
    let rc = unsafe { FwpmFilterAdd0(e.0, &filter, std::ptr::null_mut(), std::ptr::null_mut()) };
    if rc != 0 && rc != FWP_E_ALREADY_EXISTS {
        return Err(wfp_err("FwpmFilterAdd0", rc));
    }
    Ok(())
}

/// Every filter key under nub's provider at `layer`. Paged 256 at a time; each returned
/// batch is freed, and the enum handle is destroyed on both the success and error paths.
fn enum_nub_filter_keys(e: &Engine, layer: GUID) -> io::Result<Vec<GUID>> {
    let mut provider_key = NUB_PROVIDER_KEY;
    let template = FWPM_FILTER_ENUM_TEMPLATE0 {
        providerKey: &mut provider_key,
        layerKey: layer,
        enumType: FWP_FILTER_ENUM_OVERLAPPING,
        actionMask: 0xFFFF_FFFF,
        ..Default::default()
    };

    let mut enum_handle: HANDLE = std::ptr::null_mut();
    // SAFETY: `template` outlives the create; `enum_handle` is a valid out-slot.
    let rc = unsafe { FwpmFilterCreateEnumHandle0(e.0, &template, &mut enum_handle) };
    if rc != 0 {
        return Err(wfp_err("FwpmFilterCreateEnumHandle0", rc));
    }

    const PAGE: u32 = 256;
    let mut keys = Vec::new();
    let result = loop {
        let mut entries: *mut *mut FWPM_FILTER0 = std::ptr::null_mut();
        let mut returned: u32 = 0;
        // SAFETY: valid engine + enum handles; both out-params are live slots.
        let rc = unsafe { FwpmFilterEnum0(e.0, enum_handle, PAGE, &mut entries, &mut returned) };
        if rc != 0 {
            break Err(wfp_err("FwpmFilterEnum0", rc));
        }
        if !entries.is_null() {
            // SAFETY: WFP guarantees `returned` valid `*mut FWPM_FILTER0` entries.
            let slice = unsafe { std::slice::from_raw_parts(entries, returned as usize) };
            for &f in slice {
                if !f.is_null() {
                    // SAFETY: each entry is a live filter for the life of this batch.
                    keys.push(unsafe { (*f).filterKey });
                }
            }
            // Every batch is freed, including an empty one.
            unsafe { FwpmFreeMemory0(std::ptr::from_mut(&mut entries).cast()) };
        }
        if returned < PAGE {
            break Ok(());
        }
    };

    // SAFETY: destroy the enum handle on BOTH paths before returning.
    unsafe { FwpmFilterDestroyEnumHandle0(e.0, enum_handle) };
    result?;
    Ok(keys)
}

fn delete_nub_filters(e: &Engine) -> io::Result<()> {
    for layer in [
        FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        FWPM_LAYER_ALE_AUTH_CONNECT_V6,
    ] {
        for key in enum_nub_filter_keys(e, layer)? {
            // SAFETY: `key` came out of the enumeration above.
            let rc = unsafe { FwpmFilterDeleteByKey0(e.0, &key) };
            if rc != 0 && rc != FWP_E_FILTER_NOT_FOUND {
                return Err(wfp_err("FwpmFilterDeleteByKey0", rc));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `windows_sys::core::GUID` derives only `Copy`/`Clone`/`Default`, so equality is
    /// field-wise.
    fn same_guid(a: &GUID, b: &GUID) -> bool {
        a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
    }

    /// Both access-denied spellings must read as "not elevated" — BFE returns either, and
    /// treating one as a hard fault would turn a missing-elevation into a confusing crash.
    #[test]
    fn both_access_denied_spellings_are_recognized() {
        assert!(is_access_denied(FWP_E_ACCESS_DENIED));
        assert!(is_access_denied(ERROR_ACCESS_DENIED));
        assert!(!is_access_denied(FWP_E_ALREADY_EXISTS));
    }

    /// The fence is exactly four filters — two permits, two blocks, one pair per address
    /// family. A missing v6 block is a silent egress hole on a dual-stack host.
    #[test]
    fn filter_table_covers_both_families_in_both_directions() {
        let permits = FilterSpec::ALL.iter().filter(|s| s.is_permit()).count();
        assert_eq!(permits, 2);
        assert_eq!(FilterSpec::ALL.len() - permits, 2);
        let v4 = FilterSpec::ALL
            .iter()
            .filter(|s| same_guid(&s.layer(), &FWPM_LAYER_ALE_AUTH_CONNECT_V4))
            .count();
        assert_eq!(v4, 2, "one permit + one block on IPv4");
    }
}

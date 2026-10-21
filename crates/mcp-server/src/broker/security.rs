#[cfg(any(windows, test))]
use std::io;
#[cfg(any(windows, test))]
use std::path::Path;

#[cfg(any(unix, windows))]
use interprocess::local_socket::tokio::Stream as TokioStream;
#[cfg(any(unix, windows))]
use interprocess::local_socket::traits::StreamCommon as _;
#[cfg(windows)]
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
#[cfg(windows)]
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
#[cfg(windows)]
use widestring::U16CString;
#[cfg(windows)]
use win_security_identifier::{GetCurrentSid, SecurityIdentifier};

#[cfg(windows)]
pub(crate) fn pipe_security_descriptor_for_current_user() -> io::Result<SecurityDescriptor> {
    let sid = current_user_sid_string()?;
    let sddl = pipe_security_sddl_for_user_sid(&sid)?;
    let sddl = U16CString::from_str(&sddl).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("pipe security descriptor contains an interior NUL: {e}"),
        )
    })?;
    SecurityDescriptor::deserialize(sddl.as_ucstr())
}

#[cfg(windows)]
fn current_user_sid_string() -> io::Result<String> {
    SecurityIdentifier::get_current_user_sid()
        .map(|sid| sid.to_string())
        .map_err(|e| io::Error::other(format!("could not read current Windows user SID: {e}")))
}

#[cfg(any(windows, test))]
pub(crate) fn pipe_security_sddl_for_user_sid(sid: &str) -> io::Result<String> {
    validate_sid(sid)?;
    Ok(format!("O:{sid}D:P(A;;GA;;;{sid})"))
}

#[cfg(any(windows, test))]
fn validate_sid(sid: &str) -> io::Result<()> {
    if !sid.starts_with("S-1-") {
        return invalid_sid(sid);
    }
    if sid.trim() != sid {
        return invalid_sid(sid);
    }
    if sid.contains([';', '(', ')']) {
        return invalid_sid(sid);
    }

    let mut parts = sid.split('-');
    let Some("S") = parts.next() else {
        return invalid_sid(sid);
    };
    let Some("1") = parts.next() else {
        return invalid_sid(sid);
    };
    let mut authorities = 0usize;
    for part in parts {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return invalid_sid(sid);
        }
        authorities += 1;
    }
    if authorities < 2 {
        return invalid_sid(sid);
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn invalid_sid<T>(sid: &str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid Windows SID: {sid}")))
}

// Trust policy for the Windows named-pipe squatting defence. The deterministic
// pipe name (`bsl-mcp-<digest>.sock`) is raceable: a hostile local user can
// pre-create the pipe with their own DACL and win the bind. After a successful
// connect we verify the server is "us": same image path AND same user SID.
// `interprocess::peer_creds()` gives us the server PID; `sysinfo` then reads
// that PID's exe + owner SID through safe APIs (no project-local unsafe, no
// handwritten Win32 FFI). Any introspection failure or mismatch fails closed.

/// Reported identity of a backend peer, used by the trust gate.
///
/// `None` for any field means "unavailable" — the policy treats any missing
/// field on either side as a rejection (fail closed).
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PeerIdentity {
    /// Absolute path of the server executable, as the kernel reports it.
    pub exe: Option<String>,
    /// Stable user identifier (Windows SID string, Unix UID string) of the
    /// server process owner.
    pub user: Option<String>,
}

/// Pure trust decision: is a peer's reported identity trusted against the
/// expected one?
///
/// Every field of `expected` must be present and must equal the corresponding
/// field of `actual`. Missing metadata on either side rejects. The function
/// performs no I/O and no platform calls, so the policy is unit-testable on
/// platforms that don't expose the underlying introspection APIs.
#[cfg(any(windows, test))]
pub(crate) fn peer_identity_trusted(expected: &PeerIdentity, actual: &PeerIdentity) -> bool {
    let exe_ok = match (expected.exe.as_deref(), actual.exe.as_deref()) {
        (Some(exp), Some(act)) => exp == act,
        _ => false,
    };
    let user_ok = match (expected.user.as_deref(), actual.user.as_deref()) {
        (Some(exp), Some(act)) => exp == act,
        _ => false,
    };
    exe_ok && user_ok
}

/// Pure construction of a [`PeerIdentity`] from raw introspection outputs.
///
/// Extracted as a named seam so the conversion (path → string, no-op for
/// already-string SIDs) is unit-testable independently of any platform API.
#[cfg(any(windows, test))]
pub(crate) fn peer_identity_from_raw(exe: Option<&Path>, user: Option<&str>) -> PeerIdentity {
    PeerIdentity {
        exe: exe.map(|p| p.to_string_lossy().into_owned()),
        user: user.map(str::to_owned),
    }
}

/// Verify a connected named-pipe server is "us": same image path AND same user
/// SID as the current process.
///
/// This is the Windows trust gate called by both `daemon::probe_live` and
/// `proxy::connect_with_launch` after a successful `connect`. It reads the
/// server PID via `interprocess`'s safe `peer_creds()` API and then uses
/// `sysinfo` to introspect that PID's exe path and owner SID — no
/// project-local `unsafe`, no handwritten Win32 FFI. Any step that cannot
/// produce a definitive "same exe, same user" answer fails closed.
#[cfg(windows)]
pub(crate) fn verify_pipe_server_trusted(conn: &TokioStream) -> bool {
    let pid = match server_pid(conn) {
        Some(pid) => pid,
        None => return false,
    };
    let expected = match current_process_identity() {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "could not build current-process identity for trust check");
            return false;
        }
    };
    let actual = match peer_identity_for_pid(pid) {
        Some(id) => id,
        None => {
            tracing::warn!(
                peer_pid = pid,
                "could not introspect named-pipe server process; rejecting unverified stream"
            );
            return false;
        }
    };
    if peer_identity_trusted(&expected, &actual) {
        true
    } else {
        tracing::warn!(
            expected_exe = ?expected.exe,
            expected_user = ?expected.user,
            actual_exe = ?actual.exe,
            actual_user = ?actual.user,
            "named-pipe server identity does not match the current process; dropping unverified stream"
        );
        false
    }
}

/// The one spelling of [`SUPERVISED_PID_PLATFORMS`], in the only form that can serve both a
/// value and an attribute.
///
/// `names` expands to the slice; `gate` prefixes an item with the matching `#[cfg]`. The gate
/// has to come from here because `cfg!` is an expression and its argument is never
/// macro-expanded, so an attribute cannot read a list any other way — which is how the list
/// used to acquire copies that drifted from it in silence.
macro_rules! cfg_supervised_pid {
    (@platforms $mode:tt $($rest:tt)*) => {
        $crate::broker::security::cfg_supervised_pid! { @$mode
            ["android", "freebsd", "fuchsia", "linux", "netbsd", "openbsd", "redox", "windows"]
            $($rest)*
        }
    };
    (@names [$($os:tt),*]) => { &[$($os),*] };
    (@gate [$($os:tt),*] $item:item) => {
        #[cfg(any($(target_os = $os),*))]
        $item
    };
    (names) => { $crate::broker::security::cfg_supervised_pid!(@platforms names) };
    (gate $item:item) => { $crate::broker::security::cfg_supervised_pid! { @platforms gate $item } };
}
pub(crate) use cfg_supervised_pid;

/// Platforms whose peer credentials carry the peer's PID, which is the identity the supervised
/// broker pins to.
///
/// Taken from what `interprocess` actually returns from `PeerCreds::pid()`, not from its prose:
/// the implementation reads Windows pipe credentials, `ucred` on Linux/Android/OpenBSD and their
/// shape-alikes, `cr_pid` on FreeBSD and `unp_pid` on NetBSD. Darwin's `xucred` and DragonFly's
/// carry no PID at all, so pinning there cannot be verified and the mode is refused up front
/// instead of failing every connection as an identity mismatch.
///
/// This list is the single source: [`peer_pid_available`] answers the CLI gate from it, the
/// contract publishes it, and code that can only exist where a peer PID is readable is gated on
/// it through [`cfg_supervised_pid`]. A platform added to one and forgotten in another is not
/// expressible.
pub const SUPERVISED_PID_PLATFORMS: &[&str] = cfg_supervised_pid!(names);

/// Whether the running platform's peer credentials carry the peer's PID.
pub fn peer_pid_available() -> bool {
    SUPERVISED_PID_PLATFORMS.contains(&std::env::consts::OS)
}

/// Why a supervised connection was refused, carrying what the socket actually answered with.
///
/// A caller reports this to whoever passed the PID, so it names the owner it found: "not the
/// process you named" and "no process at all" send a supervisor to different places, and so does
/// learning that the workspace already had a backend of its own.
#[cfg(any(unix, windows))]
pub(crate) enum SupervisedMismatch {
    /// The socket is owned by this other process — typically a backend that was already running
    /// when the supervised daemon started, which then lost the bind and exited.
    OtherPid(u32),
    /// The peer's identity could not be established at all.
    Unknown,
}

/// Verify that a broker connection terminates at the exact backend process a
/// supervisor launched.
///
/// The PID check is the supervised-mode identity. The ordinary transport trust
/// checks still apply as defense in depth: same effective user on Unix and the
/// existing image/owner check on Windows.
///
/// A PID names a process only while that process lives: the OS reuses the number afterwards, and
/// nothing in the credentials carries a start time or generation to tell the reuse apart. What
/// this proves is that the socket is answered by the process holding that number now, run by the
/// same user, from the same image — not that it is the very process the supervisor spawned.
#[cfg(any(unix, windows))]
pub(crate) fn verify_supervised_backend(
    conn: &TokioStream,
    expected_pid: u32,
) -> Result<(), SupervisedMismatch> {
    let actual_pid = conn.peer_creds().ok().and_then(|creds| creds.pid());
    #[cfg(unix)]
    let actual_pid = actual_pid.and_then(|pid| u32::try_from(pid).ok());
    if actual_pid != Some(expected_pid) {
        tracing::warn!(
            expected_pid,
            actual_pid,
            "broker backend PID does not match the supervised process"
        );
        return Err(match actual_pid {
            Some(pid) => SupervisedMismatch::OtherPid(pid),
            None => SupervisedMismatch::Unknown,
        });
    }

    #[cfg(unix)]
    let trusted = match conn.peer_creds() {
        Ok(creds) => creds.euid().is_some_and(|uid| uid == crate::broker::name::current_euid()),
        Err(_) => false,
    };
    #[cfg(windows)]
    let trusted = verify_pipe_server_trusted(conn);

    if trusted {
        Ok(())
    } else {
        Err(SupervisedMismatch::Unknown)
    }
}

#[cfg(windows)]
fn server_pid(conn: &TokioStream) -> Option<u32> {
    match conn.peer_creds() {
        Ok(creds) => match creds.pid() {
            Some(pid) => Some(pid),
            None => {
                tracing::warn!(
                    "named-pipe server did not report a PID; rejecting unverified stream"
                );
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "could not read named-pipe peer PID; rejecting unverified stream");
            None
        }
    }
}

/// Build the [`PeerIdentity`] we expect any trusted backend to match: the
/// current process image path and the current user's SID.
#[cfg(windows)]
fn current_process_identity() -> io::Result<PeerIdentity> {
    let exe = std::env::current_exe()
        .map_err(|e| io::Error::other(format!("current_exe failed: {e}")))?;
    let user = current_user_sid_string()?;
    Ok(peer_identity_from_raw(Some(&exe), Some(&user)))
}

/// Introspect a running PID's image path and owner SID via `sysinfo`.
///
/// Returns `None` if the PID is gone, the system cannot report the requested
/// fields, or `sysinfo` itself is unavailable — all treated as fail-closed by
/// the caller.
#[cfg(windows)]
fn peer_identity_for_pid(pid: u32) -> Option<PeerIdentity> {
    let mut sys = System::new();
    // The 2-arg `refresh_processes` does not populate `user_id`; we must use
    // `refresh_processes_specifics` with an explicit `ProcessRefreshKind` that
    // asks for exe AND user.
    let kind =
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always).with_user(UpdateKind::Always);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true, kind);
    let process = sys.process(Pid::from_u32(pid))?;
    let exe = process.exe().map(Path::to_path_buf);
    let user = process.user_id().map(|uid| uid.to_string());
    Some(peer_identity_from_raw(exe.as_deref(), user.as_deref()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        peer_identity_from_raw, peer_identity_trusted, pipe_security_sddl_for_user_sid,
        PeerIdentity,
    };

    #[test]
    fn pipe_security_sddl_allows_only_exact_user_sid() {
        let sddl = pipe_security_sddl_for_user_sid("S-1-5-21-1-2-3-1001").unwrap();

        assert_eq!(sddl, "O:S-1-5-21-1-2-3-1001D:P(A;;GA;;;S-1-5-21-1-2-3-1001)");
        for forbidden in ["WD", "AN", "AU", "BU", "IU", "BA", "SY", "OW", "CO"] {
            assert!(!sddl.contains(forbidden), "SDDL must not contain {forbidden}: {sddl}");
        }
    }

    #[test]
    fn pipe_security_sddl_rejects_invalid_sid_strings() {
        for sid in [
            "",
            " ",
            "S-1-5-21-1-2-3-1001 ",
            "S-1-5-21-1-2-3-1001;WD",
            "S-1-5-21-1-2-3-1001)",
            "S-1-5-21-1-2-3-abc",
            "S-5-21-1-2-3-1001",
            "X-1-5-21-1-2-3-1001",
        ] {
            assert!(pipe_security_sddl_for_user_sid(sid).is_err(), "SID must be rejected: {sid}");
        }
    }

    fn identity(exe: &str, user: &str) -> PeerIdentity {
        PeerIdentity { exe: Some(exe.to_owned()), user: Some(user.to_owned()) }
    }

    #[test]
    fn peer_identity_trusted_accepts_exact_exe_and_user_match() {
        let expected = identity("/usr/local/bin/bsl-analyzer", "S-1-5-21-1-2-3-1001");
        let actual = identity("/usr/local/bin/bsl-analyzer", "S-1-5-21-1-2-3-1001");

        assert!(peer_identity_trusted(&expected, &actual));
    }

    #[test]
    fn peer_identity_trusted_rejects_exe_mismatch() {
        let expected = identity("/usr/local/bin/bsl-analyzer", "S-1-5-21-1-2-3-1001");
        let actual = identity("/usr/local/bin/hostile", "S-1-5-21-1-2-3-1001");

        assert!(!peer_identity_trusted(&expected, &actual));
    }

    #[test]
    fn peer_identity_trusted_rejects_user_mismatch() {
        let expected = identity("/usr/local/bin/bsl-analyzer", "S-1-5-21-1-2-3-1001");
        let actual = identity("/usr/local/bin/bsl-analyzer", "S-1-5-21-9-9-9-4242");

        assert!(!peer_identity_trusted(&expected, &actual));
    }

    #[test]
    fn peer_identity_trusted_rejects_when_actual_metadata_is_missing() {
        let expected = identity("/usr/local/bin/bsl-analyzer", "S-1-5-21-1-2-3-1001");

        let no_exe = PeerIdentity { exe: None, user: Some("S-1-5-21-1-2-3-1001".to_owned()) };
        let no_user =
            PeerIdentity { exe: Some("/usr/local/bin/bsl-analyzer".to_owned()), user: None };
        let empty = PeerIdentity::default();

        assert!(!peer_identity_trusted(&expected, &no_exe), "missing actual exe must reject");
        assert!(!peer_identity_trusted(&expected, &no_user), "missing actual user must reject");
        assert!(!peer_identity_trusted(&expected, &empty), "all metadata missing must reject");
    }

    #[test]
    fn peer_identity_trusted_rejects_when_expected_metadata_is_missing() {
        // If the caller cannot state what they expect, they cannot authenticate.
        let actual = identity("/usr/local/bin/bsl-analyzer", "S-1-5-21-1-2-3-1001");

        let expected_no_exe =
            PeerIdentity { exe: None, user: Some("S-1-5-21-1-2-3-1001".to_owned()) };
        let expected_no_user =
            PeerIdentity { exe: Some("/usr/local/bin/bsl-analyzer".to_owned()), user: None };

        assert!(
            !peer_identity_trusted(&expected_no_exe, &actual),
            "missing expected exe must reject"
        );
        assert!(
            !peer_identity_trusted(&expected_no_user, &actual),
            "missing expected user must reject"
        );
    }

    #[test]
    fn peer_identity_from_raw_carries_both_fields_when_present() {
        let exe = PathBuf::from("/usr/local/bin/bsl-analyzer");
        let id = peer_identity_from_raw(Some(&exe), Some("S-1-5-21-1-2-3-1001"));

        assert_eq!(id.exe.as_deref(), Some("/usr/local/bin/bsl-analyzer"));
        assert_eq!(id.user.as_deref(), Some("S-1-5-21-1-2-3-1001"));
    }

    #[test]
    fn peer_identity_from_raw_yields_missing_fields_when_input_is_none() {
        let exe = PathBuf::from("/usr/local/bin/bsl-analyzer");

        let no_exe = peer_identity_from_raw(None, Some("S-1-5-21-1-2-3-1001"));
        let no_user = peer_identity_from_raw(Some(&exe), None);
        let both_missing = peer_identity_from_raw(None, None);

        assert!(no_exe.exe.is_none() && no_exe.user.is_some());
        assert!(no_user.exe.is_some() && no_user.user.is_none());
        assert!(both_missing.exe.is_none() && both_missing.user.is_none());
    }

    #[test]
    fn peer_identity_from_raw_then_trusted_accepts_a_self_match() {
        // Round-trip: raw inputs → PeerIdentity → compared against an
        // equivalent identity → trusted. This pins the contract between the
        // pure conversion seam and the pure decision seam.
        let exe = PathBuf::from("/usr/local/bin/bsl-analyzer");
        let expected = peer_identity_from_raw(Some(&exe), Some("S-1-5-21-1-2-3-1001"));
        let actual = peer_identity_from_raw(Some(&exe), Some("S-1-5-21-1-2-3-1001"));

        assert!(peer_identity_trusted(&expected, &actual));
    }

    /// The list is what the CLI refuses the supervised mode by, so it has to answer for the
    /// platform actually running. Darwin is the case that matters: `xucred` carries no PID, and
    /// claiming otherwise turns every connection there into an identity mismatch.
    ///
    /// The expectation is spelled from the OS name rather than from the list, so it is an
    /// independent statement of the same fact rather than the list read back to itself.
    #[test]
    fn peer_pid_availability_answers_for_the_running_platform() {
        let carries_pid =
            !matches!(std::env::consts::OS, "macos" | "ios" | "tvos" | "watchos" | "dragonfly");

        assert_eq!(
            super::peer_pid_available(),
            carries_pid,
            "peer credentials on {} carry a PID: {carries_pid}",
            std::env::consts::OS
        );
    }

    /// The list is exactly the platforms whose peer credentials carry a PID — no more and no
    /// fewer — stated here so it holds on whatever host runs the suite.
    ///
    /// Checking only the running platform leaves a wrong entry to be found by running the
    /// tests on that very platform: the platform whose answer already went wrong and, where
    /// the entry was dropped rather than added, whose own gated tests disappeared along with
    /// it. Equality rather than membership, because a name in neither column — a target added
    /// to the list and to nothing else — would otherwise be published by the contract as
    /// supported while nothing had ever read its credentials.
    ///
    /// The expectation is written out rather than read back out of the list, which would pass
    /// whatever the list said.
    #[test]
    fn the_list_is_exactly_the_platforms_whose_credentials_carry_a_pid() {
        assert_eq!(
            super::SUPERVISED_PID_PLATFORMS,
            ["android", "freebsd", "fuchsia", "linux", "netbsd", "openbsd", "redox", "windows"],
            "a platform belongs here only once `PeerCreds::pid()` is known to answer on it",
        );
    }
}

const OFFLINE_ACCOUNT: &str = "CodexSandboxOffline";
const ONLINE_ACCOUNT: &str = "CodexSandboxOnline";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CodexBoundary {
    Managed,
    ExternalRestricted,
    ExternalEnabled,
}

pub(super) fn detect() -> CodexBoundary {
    detect_with(current_token_is_member)
}

fn detect_with(mut is_member: impl FnMut(&str) -> Option<bool>) -> CodexBoundary {
    match (is_member(OFFLINE_ACCOUNT), is_member(ONLINE_ACCOUNT)) {
        (Some(true), _) => CodexBoundary::ExternalRestricted,
        (_, Some(true)) => CodexBoundary::ExternalEnabled,
        _ => CodexBoundary::Managed,
    }
}

#[cfg(not(windows))]
fn current_token_is_member(_account: &str) -> Option<bool> {
    Some(false)
}

#[cfg(windows)]
fn current_token_is_member(account: &str) -> Option<bool> {
    windows_token::is_member(account)
}

#[cfg(windows)]
mod windows_token {
    use std::ffi::{OsStr, c_void};
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError};
    use windows_sys::Win32::Security::{CheckTokenMembership, LookupAccountNameW, SID_NAME_USE};

    pub(super) fn is_member(account: &str) -> Option<bool> {
        let account = OsStr::new(account)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut sid_len = 0;
        let mut domain_len = 0;
        let mut sid_use: SID_NAME_USE = 0;
        let first = unsafe {
            LookupAccountNameW(
                ptr::null(),
                account.as_ptr(),
                ptr::null_mut(),
                &mut sid_len,
                ptr::null_mut(),
                &mut domain_len,
                &mut sid_use,
            )
        };
        if first != 0 || sid_len == 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }

        let mut sid = vec![0u8; sid_len as usize];
        let mut domain = vec![0u16; domain_len as usize];
        let domain_ptr = if domain.is_empty() {
            ptr::null_mut()
        } else {
            domain.as_mut_ptr()
        };
        let resolved = unsafe {
            LookupAccountNameW(
                ptr::null(),
                account.as_ptr(),
                sid.as_mut_ptr().cast::<c_void>(),
                &mut sid_len,
                domain_ptr,
                &mut domain_len,
                &mut sid_use,
            )
        };
        if resolved == 0 {
            return None;
        }

        let mut member = 0;
        let checked = unsafe {
            CheckTokenMembership(
                ptr::null_mut(),
                sid.as_mut_ptr().cast::<c_void>(),
                &mut member,
            )
        };
        (checked != 0).then_some(member != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_maps_offline_online_and_unknown_tokens() {
        assert_eq!(
            detect_with(|account| match account {
                OFFLINE_ACCOUNT => Some(true),
                ONLINE_ACCOUNT => Some(false),
                _ => None,
            }),
            CodexBoundary::ExternalRestricted
        );
        assert_eq!(
            detect_with(|account| match account {
                OFFLINE_ACCOUNT => Some(false),
                ONLINE_ACCOUNT => Some(true),
                _ => None,
            }),
            CodexBoundary::ExternalEnabled
        );
        assert_eq!(detect_with(|_| Some(false)), CodexBoundary::Managed);
        assert_eq!(detect_with(|_| None), CodexBoundary::Managed);
        assert_eq!(
            detect_with(|account| match account {
                OFFLINE_ACCOUNT => None,
                ONLINE_ACCOUNT => Some(true),
                _ => None,
            }),
            CodexBoundary::ExternalEnabled
        );
    }
}

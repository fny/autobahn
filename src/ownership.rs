//! Resolution of ownership specifications to numeric IDs.
//!
//! Owner and group specifications accept a name (`www-data`), a plain
//! numeric ID (`1000`), or an explicitly numeric form (`id:1000`). Names
//! are resolved on the machine that applies them — each endpoint resolves
//! against its own passwd and group databases, which is what makes a name
//! in a fanned-out configuration mean the right thing on every host.

use anyhow::{bail, Context, Result};

/// Resolves an owner specification to a user ID.
pub fn resolve_user(spec: &str) -> Result<u32> {
    if let Some(id) = numeric(spec) {
        return Ok(id);
    }
    lookup(spec, true).with_context(|| format!("unable to resolve user '{spec}'"))
}

/// Resolves a group specification to a group ID.
pub fn resolve_group(spec: &str) -> Result<u32> {
    if let Some(id) = numeric(spec) {
        return Ok(id);
    }
    lookup(spec, false).with_context(|| format!("unable to resolve group '{spec}'"))
}

/// Parses the numeric specification forms (`1000`, `id:1000`).
/// `u32::MAX` is rejected: it is the chown sentinel for "leave this ID
/// unchanged", so accepting it would silently apply nothing.
fn numeric(spec: &str) -> Option<u32> {
    spec.strip_prefix("id:")
        .unwrap_or(spec)
        .parse()
        .ok()
        .filter(|&id| id != u32::MAX)
}

/// Looks a name up in the passwd (or group) database via the reentrant
/// libc interfaces, growing the scratch buffer as the platform demands.
fn lookup(name: &str, user: bool) -> Result<u32> {
    let name = std::ffi::CString::new(name).context("name contains an interior NUL byte")?;
    let mut capacity = 1024usize;
    loop {
        let mut buffer = vec![0u8; capacity];
        if user {
            let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            let status = unsafe {
                libc::getpwnam_r(
                    name.as_ptr(),
                    &mut passwd,
                    buffer.as_mut_ptr() as *mut libc::c_char,
                    buffer.len(),
                    &mut result,
                )
            };
            if status == libc::ERANGE {
                capacity *= 2;
                continue;
            }
            if status != 0 {
                bail!(std::io::Error::from_raw_os_error(status));
            }
            if result.is_null() {
                bail!("no such user");
            }
            return Ok(unsafe { (*result).pw_uid });
        } else {
            let mut group: libc::group = unsafe { std::mem::zeroed() };
            let mut result: *mut libc::group = std::ptr::null_mut();
            let status = unsafe {
                libc::getgrnam_r(
                    name.as_ptr(),
                    &mut group,
                    buffer.as_mut_ptr() as *mut libc::c_char,
                    buffer.len(),
                    &mut result,
                )
            };
            if status == libc::ERANGE {
                capacity *= 2;
                continue;
            }
            if status != 0 {
                bail!(std::io::Error::from_raw_os_error(status));
            }
            if result.is_null() {
                bail!("no such group");
            }
            return Ok(unsafe { (*result).gr_gid });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_forms_resolve_without_database_lookups() {
        assert_eq!(resolve_user("1000").unwrap(), 1000);
        assert_eq!(resolve_user("id:0").unwrap(), 0);
        assert_eq!(resolve_group("id:33").unwrap(), 33);
    }

    #[test]
    fn the_root_user_resolves_by_name() {
        assert_eq!(resolve_user("root").unwrap(), 0);
        assert_eq!(resolve_group("root").unwrap(), 0);
    }

    #[test]
    fn unknown_names_are_errors() {
        assert!(resolve_user("no-such-user-exists-here").is_err());
        assert!(resolve_group("no-such-group-exists-here").is_err());
    }
}

//! Bounded, reentrant NFSv4 owner/group identity resolution.

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

const CACHE_CAPACITY: usize = 1024;
const DEFAULT_NSS_BUFFER: usize = 16 * 1024;
const MAX_NSS_BUFFER: usize = 1024 * 1024;

#[derive(Default)]
struct IdentityCache {
    values: HashMap<(String, bool), Option<u32>>,
    order: VecDeque<(String, bool)>,
}

impl IdentityCache {
    fn get(&self, key: &(String, bool)) -> Option<Option<u32>> {
        self.values.get(key).copied()
    }

    fn insert(&mut self, key: (String, bool), value: Option<u32>) {
        if let Some(cached) = self.values.get_mut(&key) {
            *cached = value;
            return;
        }
        while self.values.len() >= CACHE_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.values.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.values.insert(key, value);
    }
}

fn cache() -> &'static Mutex<IdentityCache> {
    static CACHE: OnceLock<Mutex<IdentityCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(IdentityCache::default()))
}

/// Resolve an NFSv4 owner/group value to a numeric id.
///
/// Qualified values are deliberately passed to NSS intact. Stripping an
/// arbitrary `@domain` can map a remote principal to an unrelated local
/// account with the same short name.
pub(crate) fn name_to_id(value: &[u8], is_group: bool) -> Option<u32> {
    let name = std::str::from_utf8(value).ok()?.trim().to_owned();
    if let Ok(id) = name.parse::<u32>() {
        return Some(id);
    }
    let key = (name, is_group);
    if let Some(value) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return value;
    }
    let value = lookup(&key.0, is_group);
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, value);
    value
}

fn initial_buffer_size(is_group: bool) -> usize {
    let selector = if is_group {
        libc::_SC_GETGR_R_SIZE_MAX
    } else {
        libc::_SC_GETPW_R_SIZE_MAX
    };
    let suggested = unsafe { libc::sysconf(selector) };
    if suggested > 0 {
        usize::try_from(suggested)
            .unwrap_or(DEFAULT_NSS_BUFFER)
            .clamp(1024, MAX_NSS_BUFFER)
    } else {
        DEFAULT_NSS_BUFFER
    }
}

fn lookup(name: &str, is_group: bool) -> Option<u32> {
    let name = CString::new(name).ok()?;
    let mut size = initial_buffer_size(is_group);
    loop {
        let mut buffer = vec![0u8; size];
        let result = if is_group {
            let mut entry = std::mem::MaybeUninit::<libc::group>::uninit();
            let mut found = std::ptr::null_mut();
            let status = unsafe {
                libc::getgrnam_r(
                    name.as_ptr(),
                    entry.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut found,
                )
            };
            if status == 0 && !found.is_null() {
                Some(unsafe { (*found).gr_gid })
            } else if status == libc::ERANGE {
                None
            } else {
                return None;
            }
        } else {
            let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
            let mut found = std::ptr::null_mut();
            let status = unsafe {
                libc::getpwnam_r(
                    name.as_ptr(),
                    entry.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut found,
                )
            };
            if status == 0 && !found.is_null() {
                Some(unsafe { (*found).pw_uid })
            } else if status == libc::ERANGE {
                None
            } else {
                return None;
            }
        };
        if let Some(id) = result {
            return Some(id);
        }
        if size >= MAX_NSS_BUFFER {
            return None;
        }
        size = size.saturating_mul(2).min(MAX_NSS_BUFFER);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_ids_do_not_require_nss() {
        assert_eq!(name_to_id(b"4294967295", false), Some(u32::MAX));
        assert_eq!(name_to_id(b"42", true), Some(42));
    }

    #[test]
    fn qualified_names_are_not_stripped_to_local_accounts() {
        assert_eq!(name_to_id(b"root@vfsi.invalid", false), None);
    }

    #[test]
    fn cache_is_bounded() {
        let mut cache = IdentityCache::default();
        for index in 0..CACHE_CAPACITY + 37 {
            cache.insert((format!("user-{index}"), false), Some(index as u32));
        }
        assert_eq!(cache.values.len(), CACHE_CAPACITY);
        assert!(!cache.values.contains_key(&("user-0".to_owned(), false)));
        assert!(
            cache
                .values
                .contains_key(&(format!("user-{}", CACHE_CAPACITY + 36), false))
        );
    }

    #[test]
    fn reentrant_lookup_is_safe_across_threads() {
        let workers: Vec<_> = (0..16)
            .map(|_| std::thread::spawn(|| name_to_id(b"root", false)))
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), Some(0));
        }
    }
}

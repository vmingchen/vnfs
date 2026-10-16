//! Bounded, reentrant NFSv4 owner/group identity resolution.

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

const CACHE_CAPACITY: usize = 1024;
const DEFAULT_NSS_BUFFER: usize = 16 * 1024;
const MAX_NSS_BUFFER: usize = 1024 * 1024;

#[derive(Default)]
struct IdentityCache {
    // Separate namespaces allow HashMap's borrowed str lookup, so a
    // cache hit never allocates a temporary owned name.
    values: [HashMap<Box<str>, Option<u32>>; 2],
    order: VecDeque<(Box<str>, bool)>,
}

impl IdentityCache {
    fn get(&self, name: &str, is_group: bool) -> Option<Option<u32>> {
        self.values[usize::from(is_group)].get(name).copied()
    }

    fn insert(&mut self, key: (Box<str>, bool), value: Option<u32>) {
        let namespace = usize::from(key.1);
        if let Some(cached) = self.values[namespace].get_mut(key.0.as_ref()) {
            *cached = value;
            return;
        }
        // The FIFO covers both namespaces and retains the shared capacity.
        while self.order.len() >= CACHE_CAPACITY {
            if let Some((name, is_group)) = self.order.pop_front() {
                self.values[usize::from(is_group)].remove(name.as_ref());
            }
        }
        self.order.push_back(key.clone());
        self.values[namespace].insert(key.0, value);
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
    let name = std::str::from_utf8(value).ok()?.trim();
    if let Ok(id) = name.parse::<u32>() {
        return Some(id);
    }
    if let Some(value) = cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name, is_group)
    {
        return value;
    }
    let value = lookup(name, is_group);
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((Box::from(name), is_group), value);
    value
}

fn initial_buffer_size(is_group: bool) -> usize {
    let selector = if is_group {
        libc::_SC_GETGR_R_SIZE_MAX
    } else {
        libc::_SC_GETPW_R_SIZE_MAX
    };
    // SAFETY: sysconf reads only the supplied valid _SC_* selector and does not borrow
    // caller memory.
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
            // SAFETY: the CString and writable entry, result pointer and scratch
            // allocation remain live for the synchronous reentrant NSS call.
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
                // SAFETY: NSS returned success and a non-null result, proving that the
                // pointed-to entry was initialized in the still-live output storage.
                Some(unsafe { (*found).gr_gid })
            } else if status == libc::ERANGE {
                None
            } else {
                return None;
            }
        } else {
            let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
            let mut found = std::ptr::null_mut();
            // SAFETY: the CString and writable entry, result pointer and scratch
            // allocation remain live for the synchronous reentrant NSS call.
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
                // SAFETY: NSS returned success and a non-null result, proving that the
                // pointed-to entry was initialized in the still-live output storage.
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
    fn fixed_text_cache_keys_preserve_namespaces_updates_and_negative_results() {
        use std::mem::size_of;

        assert_eq!(
            size_of::<(String, bool)>() - size_of::<(Box<str>, bool)>(),
            size_of::<usize>()
        );
        let owner = (Box::<str>::from("míng@vfsi.invalid"), false);
        let group = (owner.0.clone(), true);
        let mut cache = IdentityCache::default();
        cache.insert(owner.clone(), Some(1001));
        cache.insert(group.clone(), None);
        assert_eq!(cache.get(&owner.0, owner.1), Some(Some(1001)));
        assert_eq!(cache.get(&group.0, group.1), Some(None));
        assert_eq!(cache.order.len(), 2);
        assert_eq!(cache.order[0].0.as_ref(), "míng@vfsi.invalid");

        cache.insert(owner.clone(), Some(1002));
        assert_eq!(cache.get(&owner.0, owner.1), Some(Some(1002)));
        assert_eq!(cache.order.len(), 2); // Updates keep FIFO position.
        let cloned = owner.clone();
        assert_ne!(cloned.0.as_ptr(), owner.0.as_ptr());
        drop(owner);
        assert_eq!(cache.get(&cloned.0, cloned.1), Some(Some(1002)));
    }

    #[test]
    fn allocation_cache_borrows_lookup_names_and_bounds_both_namespaces() {
        let name = "míng@vfsi.invalid";
        let mut cache = IdentityCache::default();
        cache.insert((Box::from(name), false), Some(1001));
        cache.insert((Box::from(name), true), None);
        let borrowed = String::from(name);
        assert_eq!(cache.get(&borrowed, false), Some(Some(1001)));
        assert_eq!(cache.get(&borrowed, true), Some(None));
        assert_eq!(cache.get("missing", true), None);
        cache.insert((Box::from(name), true), Some(1002));
        assert_eq!(cache.get(name, true), Some(Some(1002)));
        assert_eq!(cache.order.len(), 2);
        for index in 0..CACHE_CAPACITY {
            cache.insert(
                (format!("entry-{index}").into_boxed_str(), index % 2 != 0),
                Some(index as u32),
            );
        }
        assert_eq!(cache.order.len(), CACHE_CAPACITY);
        assert_eq!(
            cache.values.iter().map(HashMap::len).sum::<usize>(),
            CACHE_CAPACITY
        );
        assert_eq!(cache.get(name, false), None);
        assert_eq!(cache.get(name, true), None);
        assert_eq!(cache.get("entry-0", false), Some(Some(0)));
        assert_eq!(cache.get("entry-1", true), Some(Some(1)));
        assert_eq!(cache.get("entry-1", false), None);
    }

    #[test]
    fn numeric_ids_do_not_require_nss() {
        assert_eq!(name_to_id(b"4294967295", false), Some(u32::MAX));
        assert_eq!(name_to_id(b"42", true), Some(42));
        assert_eq!(name_to_id(b" 42 \n", true), Some(42));
    }

    #[test]
    fn qualified_names_are_not_stripped_to_local_accounts() {
        assert_eq!(name_to_id(b"root@vfsi.invalid", false), None);
    }

    #[test]
    fn cache_is_bounded() {
        let mut cache = IdentityCache::default();
        for index in 0..CACHE_CAPACITY + 37 {
            cache.insert(
                (format!("user-{index}").into_boxed_str(), false),
                Some(index as u32),
            );
        }
        assert_eq!(
            cache.values.iter().map(HashMap::len).sum::<usize>(),
            CACHE_CAPACITY
        );
        assert_eq!(cache.get("user-0", false), None);
        assert_eq!(
            cache.get(&format!("user-{}", CACHE_CAPACITY + 36), false),
            Some(Some((CACHE_CAPACITY + 36) as u32))
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

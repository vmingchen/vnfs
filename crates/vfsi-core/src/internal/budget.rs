use crate::{VfError, VfResult, api::ListDirOptions};
use std::path::Path;

/// Remaining traversal allocation budget. Callers decide which roots and
/// fetched entries are charged, and attach their own operation/index context.
pub struct TraversalBudget {
    entries: usize,
    bytes: usize,
}
impl TraversalBudget {
    pub fn new(entries: usize, bytes: usize) -> Self {
        Self { entries, bytes }
    }
    pub fn remaining_entries(&self) -> usize {
        self.entries
    }
    pub fn remaining_path_bytes(&self) -> usize {
        self.bytes
    }
    pub fn remaining_options(&self, options: ListDirOptions) -> ListDirOptions {
        options.max_entries(self.entries).max_path_bytes(self.bytes)
    }
    pub fn charge(&mut self, path: &Path) -> VfResult<()> {
        self.charge_paths([path])
    }
    pub fn charge_paths<'a>(&mut self, paths: impl IntoIterator<Item = &'a Path>) -> VfResult<()> {
        if self.entries == 0 {
            return Err(limit_error());
        }
        let bytes = paths.into_iter().try_fold(0usize, |total, path| {
            total
                .checked_add(path.as_os_str().len())
                .ok_or_else(limit_error)
        })?;
        self.charge_bytes(bytes)?;
        self.entries -= 1;
        Ok(())
    }
    pub fn charge_path(&mut self, path: &Path) -> VfResult<()> {
        self.charge_bytes(path.as_os_str().len())
    }
    pub fn charge_bytes(&mut self, bytes: usize) -> VfResult<()> {
        self.bytes = self.bytes.checked_sub(bytes).ok_or_else(limit_error)?;
        Ok(())
    }
}

fn limit_error() -> VfError {
    VfError::client(0, libc::EFBIG as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_path_charges_and_failed_entries_preserve_remaining_budget() {
        let mut budget = TraversalBudget::new(1, 5);
        budget.charge_path(Path::new("/a")).unwrap();
        assert_eq!(budget.remaining_entries(), 1);
        assert!(budget.charge(Path::new("/long")).is_err());
        assert_eq!(budget.remaining_entries(), 1);
        assert_eq!(
            budget
                .remaining_options(ListDirOptions::new())
                .path_byte_limit(),
            3
        );
        budget.charge(Path::new("/é")).unwrap();
        assert_eq!(budget.remaining_entries(), 0);
        assert_eq!(
            budget
                .remaining_options(ListDirOptions::new())
                .path_byte_limit(),
            0
        );
        assert!(budget.charge(Path::new("")).is_err());
    }

    #[test]
    fn maximum_limits_do_not_wrap_and_remaining_options_retain_policy() {
        let mut budget = TraversalBudget::new(usize::MAX, usize::MAX);
        budget.charge(Path::new("/file")).unwrap();
        let options = budget.remaining_options(
            ListDirOptions::new()
                .recursive(true)
                .follow_symlinks(true)
                .max_depth(7),
        );
        assert_eq!(options.entry_limit(), usize::MAX - 1);
        assert_eq!(options.path_byte_limit(), usize::MAX - 5);
        assert!(options.is_recursive());
        assert!(options.follows_symlinks());
        assert_eq!(options.depth_limit(), 7);
    }
}

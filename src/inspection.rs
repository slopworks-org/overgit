//! Command-local Git reads for status and the non-mutating doctor pass.
//!
//! Mutation repositories never opt in. Failed reads are not cached, and filtered
//! base blobs remain keyed by their exact path as well as object and mode.
use crate::{git, repo::Repo};
use anyhow::{Context as _, Result, ensure};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

type TreeRecords = BTreeMap<Vec<u8>, Vec<u8>>;
type ObjectKey = (bool, String, String, String);

#[derive(Clone, Debug, Default)]
pub(crate) struct ReadCache {
    indexes: [Option<Rc<Vec<u8>>>; 2],
    head_exists: Option<bool>,
    tree: Option<Rc<TreeRecords>>,
    objects: BTreeMap<ObjectKey, Vec<u8>>,
    object_bytes: usize,
}

impl Repo {
    pub(crate) fn for_inspection(&self) -> Self {
        let mut repo = self.clone();
        repo.inspection
            .get_or_insert_with(|| Rc::new(RefCell::new(ReadCache::default())));
        repo
    }

    pub(crate) fn read_index(&self, overlay: bool, path: Option<&str>) -> Result<Rc<Vec<u8>>> {
        let slot = usize::from(overlay);
        if let Some(cache) = &self.inspection
            && let Some(bytes) = &cache.borrow().indexes[slot]
        {
            return Ok(Rc::clone(bytes));
        }
        let mut args = vec!["ls-files", "--stage", "-z"];
        if self.inspection.is_none()
            && let Some(path) = path
        {
            args.extend(["--", path]);
        }
        let bytes = Rc::new(if overlay {
            self.overlay(args)?
        } else {
            self.base(args)?
        });
        if let Some(cache) = &self.inspection {
            cache.borrow_mut().indexes[slot] = Some(Rc::clone(&bytes));
        }
        Ok(bytes)
    }

    pub(crate) fn base_head_exists(&self) -> Result<bool> {
        if let Some(cache) = &self.inspection
            && let Some(exists) = cache.borrow().head_exists
        {
            return Ok(exists);
        }
        let head = git::output(
            self.base_cmd()
                .args(["rev-parse", "--verify", "--quiet", "HEAD"]),
            None,
        )?;
        let exists = head.status.success();
        ensure!(
            exists || head.status.code() == Some(1),
            "cannot resolve base HEAD"
        );
        if let Some(cache) = &self.inspection {
            cache.borrow_mut().head_exists = Some(exists);
        }
        Ok(exists)
    }

    pub(crate) fn base_tree_entry(&self, path: &str) -> Result<Vec<u8>> {
        let Some(cache) = &self.inspection else {
            return self.base(["ls-tree", "-z", "HEAD", "--", path]);
        };
        if cache.borrow().tree.is_none() {
            let bytes = self.base(["ls-tree", "-r", "-t", "-z", "HEAD"])?;
            let mut records = BTreeMap::new();
            for record in bytes
                .split(|byte| *byte == 0)
                .filter(|record| !record.is_empty())
            {
                let tab = record
                    .iter()
                    .position(|byte| *byte == b'\t')
                    .context("invalid tree record")?;
                records.insert(record[tab + 1..].to_vec(), record.to_vec());
            }
            cache.borrow_mut().tree = Some(Rc::new(records));
        }
        Ok(cache
            .borrow()
            .tree
            .as_ref()
            .and_then(|tree| tree.get(path.as_bytes()))
            .cloned()
            .unwrap_or_default())
    }

    pub(crate) fn read_object(
        &self,
        path: &str,
        overlay: bool,
        mode: &str,
        oid: &str,
    ) -> Result<Vec<u8>> {
        let filtered_path = if overlay || mode == "120000" {
            String::new()
        } else {
            path.to_owned()
        };
        let key = (overlay, filtered_path, mode.to_owned(), oid.to_owned());
        if let Some(cache) = &self.inspection
            && let Some(data) = cache.borrow().objects.get(&key)
        {
            return Ok(data.clone());
        }
        let data = if overlay {
            self.overlay(["cat-file", "blob", oid])?
        } else if mode == "120000" {
            self.base(["cat-file", "blob", oid])?
        } else {
            self.base(["cat-file", "--filters", &format!("--path={path}"), oid])?
        };
        if let Some(cache) = &self.inspection {
            // Diagnostics must not retain every large owned payload just to avoid
            // repeated subprocesses. Larger reads still use the same Git path.
            const OBJECT_CACHE_LIMIT: usize = 8 * 1024 * 1024;
            let mut cache = cache.borrow_mut();
            if data.len() <= OBJECT_CACHE_LIMIT.saturating_sub(cache.object_bytes) {
                cache.object_bytes += data.len();
                cache.objects.insert(key, data.clone());
            }
        }
        Ok(data)
    }
}

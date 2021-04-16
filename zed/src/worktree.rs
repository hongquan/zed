mod char_bag;
mod fuzzy;

use crate::{
    editor::{History, Snapshot as BufferSnapshot},
    sum_tree::{self, Edit, SumTree},
};
use anyhow::{anyhow, Result};
pub use fuzzy::match_paths;
use fuzzy::PathEntry;
use gpui::{scoped_pool, AppContext, Entity, ModelContext, ModelHandle, Task};
use ignore::dir::{Ignore, IgnoreBuilder};
use parking_lot::Mutex;
use smol::{channel::Sender, Timer};
use std::{collections::HashSet, future::Future};
use std::{
    ffi::OsStr,
    fmt, fs,
    io::{self, Read, Write},
    ops::AddAssign,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub use fuzzy::PathMatch;

#[derive(Debug)]
enum ScanState {
    Idle,
    Scanning,
    Err(io::Error),
}

pub struct Worktree {
    snapshot: Snapshot,
    scanner: Arc<BackgroundScanner>,
    scan_state: ScanState,
    poll_scheduled: bool,
}

#[derive(Clone)]
pub struct Snapshot {
    id: usize,
    path: Arc<Path>,
    root_inode: Option<u64>,
    entries: SumTree<Entry>,
}

#[derive(Clone)]
pub struct FileHandle {
    worktree: ModelHandle<Worktree>,
    inode: u64,
}

impl Worktree {
    pub fn new(path: impl Into<Arc<Path>>, ctx: &mut ModelContext<Self>) -> Self {
        let scan_state = smol::channel::unbounded();
        let snapshot = Snapshot {
            id: ctx.model_id(),
            path: path.into(),
            root_inode: None,
            entries: Default::default(),
        };
        let scanner = Arc::new(BackgroundScanner::new(snapshot.clone(), scan_state.0));
        let tree = Self {
            snapshot,
            scanner,
            scan_state: ScanState::Idle,
            poll_scheduled: false,
        };

        let scanner = tree.scanner.clone();
        std::thread::spawn(move || scanner.run());

        ctx.spawn_stream(scan_state.1, Self::observe_scan_state, |_, _| {})
            .detach();

        tree
    }

    fn observe_scan_state(&mut self, scan_state: ScanState, ctx: &mut ModelContext<Self>) {
        self.scan_state = scan_state;
        self.poll_entries(ctx);
    }

    fn poll_entries(&mut self, ctx: &mut ModelContext<Self>) {
        self.snapshot = self.scanner.snapshot();
        ctx.notify();

        if self.is_scanning() && !self.poll_scheduled {
            ctx.spawn(Timer::after(Duration::from_millis(100)), |this, _, ctx| {
                this.poll_scheduled = false;
                this.poll_entries(ctx);
            })
            .detach();
            self.poll_scheduled = true;
        }
    }

    fn is_scanning(&self) -> bool {
        if let ScanState::Scanning = self.scan_state {
            true
        } else {
            false
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    pub fn contains_path(&self, path: &Path) -> bool {
        path.starts_with(&self.snapshot.path)
    }

    pub fn has_inode(&self, inode: u64) -> bool {
        self.snapshot.entries.get(&inode).is_some()
    }

    pub fn file_count(&self) -> usize {
        self.snapshot.entries.summary().file_count
    }

    pub fn abs_path_for_inode(&self, ino: u64) -> Result<PathBuf> {
        let mut result = self.snapshot.path.to_path_buf();
        result.push(self.path_for_inode(ino, false)?);
        Ok(result)
    }

    pub fn path_for_inode(&self, ino: u64, include_root: bool) -> Result<PathBuf> {
        let mut components = Vec::new();
        let mut entry = self
            .snapshot
            .entries
            .get(&ino)
            .ok_or_else(|| anyhow!("entry does not exist in worktree"))?;
        components.push(entry.name());
        while let Some(parent) = entry.parent() {
            entry = self.snapshot.entries.get(&parent).unwrap();
            components.push(entry.name());
        }

        let mut components = components.into_iter().rev();
        if !include_root {
            components.next();
        }

        let mut path = PathBuf::new();
        for component in components {
            path.push(component);
        }
        Ok(path)
    }

    pub fn load_history(
        &self,
        ino: u64,
        ctx: &AppContext,
    ) -> impl Future<Output = Result<History>> {
        let path = self.abs_path_for_inode(ino);
        ctx.background_executor().spawn(async move {
            let mut file = std::fs::File::open(&path?)?;
            let mut base_text = String::new();
            file.read_to_string(&mut base_text)?;
            Ok(History::new(Arc::from(base_text)))
        })
    }

    pub fn save<'a>(
        &self,
        ino: u64,
        content: BufferSnapshot,
        ctx: &AppContext,
    ) -> Task<Result<()>> {
        let path = self.abs_path_for_inode(ino);
        ctx.background_executor().spawn(async move {
            let buffer_size = content.text_summary().bytes.min(10 * 1024);
            let file = std::fs::File::create(&path?)?;
            let mut writer = std::io::BufWriter::with_capacity(buffer_size, file);
            for chunk in content.fragments() {
                writer.write(chunk.as_bytes())?;
            }
            writer.flush()?;
            Ok(())
        })
    }

    fn fmt_entry(&self, f: &mut fmt::Formatter<'_>, ino: u64, indent: usize) -> fmt::Result {
        match self.snapshot.entries.get(&ino).unwrap() {
            Entry::Dir { name, children, .. } => {
                write!(
                    f,
                    "{}{}/ ({})\n",
                    " ".repeat(indent),
                    name.to_string_lossy(),
                    ino
                )?;
                for child_id in children.iter() {
                    self.fmt_entry(f, *child_id, indent + 2)?;
                }
                Ok(())
            }
            Entry::File { name, .. } => write!(
                f,
                "{}{} ({})\n",
                " ".repeat(indent),
                name.to_string_lossy(),
                ino
            ),
        }
    }

    #[cfg(test)]
    pub fn files<'a>(&'a self) -> impl Iterator<Item = u64> + 'a {
        self.snapshot
            .entries
            .cursor::<(), ()>()
            .filter_map(|entry| {
                if let Entry::File { inode, .. } = entry {
                    Some(*inode)
                } else {
                    None
                }
            })
    }
}

impl Entity for Worktree {
    type Event = ();
}

impl fmt::Debug for Worktree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(root_ino) = self.snapshot.root_inode {
            self.fmt_entry(f, root_ino, 0)
        } else {
            write!(f, "Empty tree\n")
        }
    }
}

impl Snapshot {
    pub fn file_count(&self) -> usize {
        self.entries.summary().file_count
    }

    pub fn root_entry(&self) -> Option<&Entry> {
        self.root_inode.and_then(|inode| self.entries.get(&inode))
    }

    fn inode_for_path(&self, path: impl AsRef<Path>) -> Option<u64> {
        let path = path.as_ref();
        self.root_inode.and_then(|mut inode| {
            'components: for path_component in path {
                if let Some(Entry::Dir { children, .. }) = &self.entries.get(&inode) {
                    for child in children.as_ref() {
                        if self.entries.get(child).map(|entry| entry.name()) == Some(path_component)
                        {
                            inode = *child;
                            continue 'components;
                        }
                    }
                }
                return None;
            }
            Some(inode)
        })
    }

    fn entry_for_path(&self, path: impl AsRef<Path>) -> Option<&Entry> {
        self.inode_for_path(path)
            .and_then(|inode| self.entries.get(&inode))
    }

    fn reparent_entry(
        &mut self,
        child_inode: u64,
        old_parent_inode: Option<u64>,
        new_parent_inode: Option<u64>,
    ) {
        let mut edits_len = 1;
        if old_parent_inode.is_some() {
            edits_len += 1;
        }
        if new_parent_inode.is_some() {
            edits_len += 1;
        }
        let mut deletions = Vec::with_capacity(edits_len);
        let mut insertions = Vec::with_capacity(edits_len);

        // Remove the entries from the sum tree.
        deletions.push(Edit::Remove(child_inode));
        if let Some(old_parent_inode) = old_parent_inode {
            deletions.push(Edit::Remove(old_parent_inode));
        }
        if let Some(new_parent_inode) = new_parent_inode {
            deletions.push(Edit::Remove(new_parent_inode));
        }
        let removed_entries = self.entries.edit(deletions);
        let mut child_entry = None;
        let mut old_parent_entry = None;
        let mut new_parent_entry = None;
        for removed_entry in removed_entries {
            if removed_entry.ino() == child_inode {
                child_entry = Some(removed_entry);
            } else if Some(removed_entry.ino()) == old_parent_inode {
                old_parent_entry = Some(removed_entry);
            } else if Some(removed_entry.ino()) == new_parent_inode {
                new_parent_entry = Some(removed_entry);
            }
        }

        // Update the child entry's parent.
        let mut child_entry = child_entry.expect("cannot reparent non-existent entry");
        child_entry.set_parent(new_parent_inode);
        insertions.push(Edit::Insert(child_entry));

        // Remove the child entry from it's old parent's children.
        if let Some(mut old_parent_entry) = old_parent_entry {
            if let Entry::Dir { children, .. } = &mut old_parent_entry {
                *children = children
                    .into_iter()
                    .cloned()
                    .filter(|c| *c != child_inode)
                    .collect();
                insertions.push(Edit::Insert(old_parent_entry));
            } else {
                panic!("snapshot entry's new parent was not a directory");
            }
        }

        // Add the child entry to it's new parent's children.
        if let Some(mut new_parent_entry) = new_parent_entry {
            if let Entry::Dir { children, .. } = &mut new_parent_entry {
                *children = children
                    .into_iter()
                    .cloned()
                    .chain(Some(child_inode))
                    .collect();
                insertions.push(Edit::Insert(new_parent_entry));
            } else {
                panic!("snapshot entry's new parent is not a directory");
            }
        }

        self.entries.edit(insertions);
    }
}

impl FileHandle {
    pub fn path(&self, ctx: &AppContext) -> PathBuf {
        self.worktree
            .read(ctx)
            .path_for_inode(self.inode, false)
            .unwrap()
    }

    pub fn load_history(&self, ctx: &AppContext) -> impl Future<Output = Result<History>> {
        self.worktree.read(ctx).load_history(self.inode, ctx)
    }

    pub fn save<'a>(&self, content: BufferSnapshot, ctx: &AppContext) -> Task<Result<()>> {
        let worktree = self.worktree.read(ctx);
        worktree.save(self.inode, content, ctx)
    }

    pub fn entry_id(&self) -> (usize, u64) {
        (self.worktree.id(), self.inode)
    }
}

#[derive(Clone, Debug)]
pub enum Entry {
    Dir {
        parent: Option<u64>,
        name: Arc<OsStr>,
        inode: u64,
        is_symlink: bool,
        is_ignored: bool,
        children: Arc<[u64]>,
        pending: bool,
    },
    File {
        parent: Option<u64>,
        name: Arc<OsStr>,
        path: PathEntry,
        inode: u64,
        is_symlink: bool,
        is_ignored: bool,
    },
}

impl Entry {
    fn ino(&self) -> u64 {
        match self {
            Entry::Dir { inode: ino, .. } => *ino,
            Entry::File { inode: ino, .. } => *ino,
        }
    }

    fn parent(&self) -> Option<u64> {
        match self {
            Entry::Dir { parent, .. } => *parent,
            Entry::File { parent, .. } => *parent,
        }
    }

    fn set_parent(&mut self, new_parent: Option<u64>) {
        match self {
            Entry::Dir { parent, .. } => *parent = new_parent,
            Entry::File { parent, .. } => *parent = new_parent,
        }
    }

    fn name(&self) -> &OsStr {
        match self {
            Entry::Dir { name, .. } => name,
            Entry::File { name, .. } => name,
        }
    }
}

impl sum_tree::Item for Entry {
    type Summary = EntrySummary;

    fn summary(&self) -> Self::Summary {
        EntrySummary {
            max_ino: self.ino(),
            file_count: if matches!(self, Self::File { .. }) {
                1
            } else {
                0
            },
        }
    }
}

impl sum_tree::KeyedItem for Entry {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.ino()
    }
}

#[derive(Clone, Debug, Default)]
pub struct EntrySummary {
    max_ino: u64,
    file_count: usize,
}

impl<'a> AddAssign<&'a EntrySummary> for EntrySummary {
    fn add_assign(&mut self, rhs: &'a EntrySummary) {
        self.max_ino = rhs.max_ino;
        self.file_count += rhs.file_count;
    }
}

impl<'a> sum_tree::Dimension<'a, EntrySummary> for u64 {
    fn add_summary(&mut self, summary: &'a EntrySummary) {
        *self = summary.max_ino;
    }
}

#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct FileCount(usize);

impl<'a> sum_tree::Dimension<'a, EntrySummary> for FileCount {
    fn add_summary(&mut self, summary: &'a EntrySummary) {
        self.0 += summary.file_count;
    }
}

struct BackgroundScanner {
    snapshot: Mutex<Snapshot>,
    notify: Sender<ScanState>,
    thread_pool: scoped_pool::Pool,
}

impl BackgroundScanner {
    fn new(snapshot: Snapshot, notify: Sender<ScanState>) -> Self {
        Self {
            snapshot: Mutex::new(snapshot),
            notify,
            thread_pool: scoped_pool::Pool::new(16),
        }
    }

    fn path(&self) -> Arc<Path> {
        self.snapshot.lock().path.clone()
    }

    fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().clone()
    }

    fn run(&self) {
        let path = {
            let mut snapshot = self.snapshot.lock();
            let canonical_path = snapshot
                .path
                .canonicalize()
                .map(Arc::from)
                .unwrap_or_else(|_| snapshot.path.clone());
            snapshot.path = canonical_path.clone();
            canonical_path
        };

        // Create the event stream before we start scanning to ensure we receive events for changes
        // that occur in the middle of the scan.
        let event_stream =
            fsevent::EventStream::new(&[path.as_ref()], Duration::from_millis(100), |events| {
                if smol::block_on(self.notify.send(ScanState::Scanning)).is_err() {
                    return false;
                }

                self.process_events(events);

                if smol::block_on(self.notify.send(ScanState::Idle)).is_err() {
                    return false;
                }

                true
            });

        if smol::block_on(self.notify.send(ScanState::Scanning)).is_err() {
            return;
        }

        if let Err(err) = self.scan_dirs() {
            if smol::block_on(self.notify.send(ScanState::Err(err))).is_err() {
                return;
            }
        }

        if smol::block_on(self.notify.send(ScanState::Idle)).is_err() {
            return;
        }

        event_stream.run();
    }

    fn scan_dirs(&self) -> io::Result<()> {
        let path = self.path();
        let metadata = fs::metadata(&path)?;
        let inode = metadata.ino();
        let is_symlink = fs::symlink_metadata(&path)?.file_type().is_symlink();
        let name = Arc::from(path.file_name().unwrap_or(OsStr::new("/")));
        let relative_path = PathBuf::from(&name);

        let mut ignore = IgnoreBuilder::new().build().add_parents(&path).unwrap();
        if metadata.is_dir() {
            ignore = ignore.add_child(&path).unwrap();
        }
        let is_ignored = ignore.matched(&path, metadata.is_dir()).is_ignore();

        if metadata.file_type().is_dir() {
            let is_ignored = is_ignored || name.as_ref() == ".git";
            let dir_entry = Entry::Dir {
                parent: None,
                name,
                inode,
                is_symlink,
                is_ignored,
                children: Arc::from([]),
                pending: true,
            };
            self.insert_entries(Some(dir_entry.clone()));
            self.snapshot.lock().root_inode = Some(inode);

            let (tx, rx) = crossbeam_channel::unbounded();

            tx.send(Ok(ScanJob {
                ino: inode,
                path: path.clone(),
                relative_path,
                dir_entry,
                ignore: Some(ignore),
                scan_queue: tx.clone(),
            }))
            .unwrap();
            drop(tx);

            let mut results = Vec::new();
            results.resize_with(self.thread_pool.workers(), || Ok(()));
            self.thread_pool.scoped(|pool| {
                for result in &mut results {
                    pool.execute(|| {
                        let result = result;
                        while let Ok(job) = rx.recv() {
                            if let Err(err) = job.and_then(|job| self.scan_dir(job, None)) {
                                *result = Err(err);
                                break;
                            }
                        }
                    });
                }
            });
            results.into_iter().collect::<io::Result<()>>()?;
        } else {
            self.insert_entries(Some(Entry::File {
                parent: None,
                name,
                path: PathEntry::new(inode, &relative_path, is_ignored),
                inode,
                is_symlink,
                is_ignored,
            }));
            self.snapshot.lock().root_inode = Some(inode);
        }

        Ok(())
    }

    fn scan_dir(&self, job: ScanJob, mut children: Option<&mut Vec<u64>>) -> io::Result<()> {
        let scan_queue = job.scan_queue;
        let mut dir_entry = job.dir_entry;

        let mut new_children = Vec::new();
        let mut new_entries = Vec::new();
        let mut new_jobs = Vec::new();

        for child_entry in fs::read_dir(&job.path)? {
            let child_entry = child_entry?;
            let name: Arc<OsStr> = child_entry.file_name().into();
            let relative_path = job.relative_path.join(name.as_ref());
            let metadata = child_entry.metadata()?;
            let ino = metadata.ino();
            let is_symlink = metadata.file_type().is_symlink();
            let path = job.path.join(name.as_ref());

            new_children.push(ino);
            if let Some(children) = children.as_mut() {
                children.push(ino);
            }
            if metadata.is_dir() {
                let mut is_ignored = true;
                let mut ignore = None;

                if let Some(parent_ignore) = job.ignore.as_ref() {
                    let child_ignore = parent_ignore.add_child(&path).unwrap();
                    is_ignored =
                        child_ignore.matched(&path, true).is_ignore() || name.as_ref() == ".git";
                    if !is_ignored {
                        ignore = Some(child_ignore);
                    }
                }

                let dir_entry = Entry::Dir {
                    parent: Some(job.ino),
                    name,
                    inode: ino,
                    is_symlink,
                    is_ignored,
                    children: Arc::from([]),
                    pending: true,
                };
                new_entries.push(dir_entry.clone());
                new_jobs.push(ScanJob {
                    ino,
                    path: Arc::from(path),
                    relative_path,
                    dir_entry,
                    ignore,
                    scan_queue: scan_queue.clone(),
                });
            } else {
                let is_ignored = job
                    .ignore
                    .as_ref()
                    .map_or(true, |i| i.matched(&path, false).is_ignore());
                new_entries.push(Entry::File {
                    parent: Some(job.ino),
                    name,
                    path: PathEntry::new(ino, &relative_path, is_ignored),
                    inode: ino,
                    is_symlink,
                    is_ignored,
                });
            };
        }

        if let Entry::Dir {
            children, pending, ..
        } = &mut dir_entry
        {
            *children = Arc::from(new_children);
            *pending = false;
        } else {
            unreachable!()
        }
        new_entries.push(dir_entry);

        self.insert_entries(new_entries);
        for new_job in new_jobs {
            scan_queue.send(Ok(new_job)).unwrap();
        }

        Ok(())
    }

    fn process_events(&self, mut events: Vec<fsevent::Event>) {
        let mut snapshot = self.snapshot();

        events.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        let mut paths = events.into_iter().map(|e| e.path).peekable();
        let mut possible_removed_inodes = HashSet::new();
        let (scan_queue_tx, scan_queue_rx) = crossbeam_channel::unbounded();

        while let Some(path) = paths.next() {
            let relative_path = match path.strip_prefix(&snapshot.path) {
                Ok(relative_path) => relative_path.to_path_buf(),
                Err(e) => {
                    log::error!("Unexpected event {:?}", e);
                    continue;
                }
            };

            let snapshot_entry = snapshot.entry_for_path(&relative_path);
            let fs_entry = self.fs_entry_for_path(&snapshot.path, &path);

            match fs_entry {
                // If this path currently exists on the filesystem, then ensure that the snapshot's
                // entry for this path is up-to-date.
                Ok(Some((fs_entry, ignore))) => {
                    let fs_inode = fs_entry.ino();
                    let fs_parent_inode = fs_entry.parent();

                    // If the snapshot already contains an entry for this path, then ensure that the
                    // entry has the correct inode and parent.
                    if let Some(snapshot_entry) = snapshot_entry {
                        let snapshot_inode = snapshot_entry.ino();
                        let snapshot_parent_inode = snapshot_entry.parent();

                        // If the snapshot entry already matches the filesystem, then skip to the
                        // next event path.
                        if snapshot_inode == fs_inode && snapshot_parent_inode == fs_parent_inode {
                            continue;
                        }

                        // If it does not match, then detach this inode from its current parent, and
                        // record that it may have been removed from the worktree.
                        snapshot.reparent_entry(snapshot_inode, snapshot_parent_inode, None);
                        possible_removed_inodes.insert(snapshot_inode);
                    }

                    // If the snapshot already contained an entry for the inode that is now located
                    // at this path in the filesystem, then move it to reflect its current parent on
                    // the filesystem.
                    if let Some(snapshot_entry_for_inode) = snapshot.entries.get(&fs_inode) {
                        let snapshot_parent_inode = snapshot_entry_for_inode.parent();
                        snapshot.reparent_entry(fs_inode, snapshot_parent_inode, fs_parent_inode);
                    }
                    // If the snapshot has no entry for this inode, then scan the filesystem to find
                    // all descendents of this new inode. Discard any subsequent events that are
                    // contained by the current path, since the directory is already being scanned
                    // from scratch.
                    else {
                        while let Some(next_path) = paths.peek() {
                            if next_path.starts_with(&path) {
                                paths.next();
                            }
                        }
                        scan_queue_tx
                            .send(Ok(ScanJob {
                                ino: fs_inode,
                                path: Arc::from(path),
                                relative_path,
                                dir_entry: fs_entry,
                                ignore: Some(ignore),
                                scan_queue: scan_queue_tx.clone(),
                            }))
                            .unwrap();
                    }
                }

                // If this path no longer exists on the filesystem, then remove it from the snapshot.
                Ok(None) => {
                    if let Some(snapshot_entry) = snapshot_entry {
                        let snapshot_inode = snapshot_entry.ino();
                        let snapshot_parent_inode = snapshot_entry.parent();
                        snapshot.reparent_entry(snapshot_inode, snapshot_parent_inode, None);
                        possible_removed_inodes.insert(snapshot_inode);
                    }
                }
                Err(e) => {
                    // TODO - create a special 'error' entry in the entries tree to mark this
                    log::error!("Error reading file on event {:?}", e);
                }
            }
        }

        // For now, update the locked snapshot at this point, because `scan_dir` uses that.
        *self.snapshot.lock() = snapshot;

        // Scan any directories that were moved into this worktree as part of this event batch.
        drop(scan_queue_tx);
        let mut scanned_inodes = Vec::new();
        scanned_inodes.resize_with(self.thread_pool.workers(), || Ok(Vec::new()));
        self.thread_pool.scoped(|pool| {
            for worker_inodes in &mut scanned_inodes {
                pool.execute(|| {
                    let worker_inodes = worker_inodes;
                    while let Ok(job) = scan_queue_rx.recv() {
                        if let Err(err) = job.and_then(|job| {
                            self.scan_dir(job, Some(worker_inodes.as_mut().unwrap()))
                        }) {
                            *worker_inodes = Err(err);
                            break;
                        }
                    }
                });
            }
        });

        // Remove any entries that became orphaned when processing this events batch.
        let mut snapshot = self.snapshot();
        let mut deletions = Vec::new();
        let mut descendent_stack = Vec::new();
        for inode in possible_removed_inodes {
            if let Some(entry) = snapshot.entries.get(&inode) {
                if entry.parent().is_none() {
                    descendent_stack.push(inode);
                }
            }

            // Recursively remove the orphaned nodes' descendants.
            while let Some(inode) = descendent_stack.pop() {
                if let Some(entry) = snapshot.entries.get(&inode) {
                    deletions.push(Edit::Remove(inode));
                    if let Entry::Dir { children, .. } = entry {
                        descendent_stack.extend_from_slice(children.as_ref());
                    }
                }
            }
        }
        snapshot.entries.edit(deletions);
        *self.snapshot.lock() = snapshot;
    }

    fn fs_entry_for_path(&self, root_path: &Path, path: &Path) -> Result<Option<(Entry, Ignore)>> {
        match fs::metadata(&path) {
            Ok(metadata) => {
                let mut ignore = IgnoreBuilder::new().build().add_parents(&path).unwrap();
                if metadata.is_dir() {
                    ignore = ignore.add_child(&path).unwrap();
                }
                let is_ignored = ignore.matched(&path, metadata.is_dir()).is_ignore();

                let inode = metadata.ino();
                let name: Arc<OsStr> = Arc::from(path.file_name().unwrap_or(OsStr::new("/")));
                let is_symlink = fs::symlink_metadata(&path)?.file_type().is_symlink();
                let parent = if path == root_path {
                    None
                } else {
                    Some(fs::metadata(path.parent().unwrap())?.ino())
                };
                if metadata.file_type().is_dir() {
                    Ok(Some((
                        Entry::Dir {
                            parent,
                            name,
                            inode,
                            is_symlink,
                            is_ignored,
                            children: Arc::from([]),
                            pending: true,
                        },
                        ignore,
                    )))
                } else {
                    Ok(Some((
                        Entry::File {
                            parent,
                            name,
                            path: PathEntry::new(
                                inode,
                                &path.strip_prefix(root_path).unwrap(),
                                is_ignored,
                            ),
                            inode,
                            is_symlink,
                            is_ignored,
                        },
                        ignore,
                    )))
                }
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(anyhow::Error::new(err))
                }
            }
        }
    }

    fn insert_entries(&self, entries: impl IntoIterator<Item = Entry>) {
        self.snapshot
            .lock()
            .entries
            .edit(entries.into_iter().map(Edit::Insert).collect::<Vec<_>>());
    }
}

struct ScanJob {
    ino: u64,
    path: Arc<Path>,
    relative_path: PathBuf,
    dir_entry: Entry,
    ignore: Option<Ignore>,
    scan_queue: crossbeam_channel::Sender<io::Result<ScanJob>>,
}

pub trait WorktreeHandle {
    fn file(&self, entry_id: u64, app: &AppContext) -> Result<FileHandle>;
}

impl WorktreeHandle for ModelHandle<Worktree> {
    fn file(&self, inode: u64, app: &AppContext) -> Result<FileHandle> {
        if self.read(app).has_inode(inode) {
            Ok(FileHandle {
                worktree: self.clone(),
                inode,
            })
        } else {
            Err(anyhow!("entry does not exist in tree"))
        }
    }
}

trait UnwrapIgnoreTuple {
    fn unwrap(self) -> Ignore;
}

impl UnwrapIgnoreTuple for (Ignore, Option<ignore::Error>) {
    fn unwrap(self) -> Ignore {
        if let Some(error) = self.1 {
            log::error!("error loading gitignore data: {}", error);
        }
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Buffer;
    use crate::test::*;
    use anyhow::Result;
    use gpui::App;
    use serde_json::json;
    use std::os::unix;

    #[test]
    fn test_populate_and_search() {
        App::test_async((), |mut app| async move {
            let dir = temp_tree(json!({
                "root": {
                    "apple": "",
                    "banana": {
                        "carrot": {
                            "date": "",
                            "endive": "",
                        }
                    },
                    "fennel": {
                        "grape": "",
                    }
                }
            }));

            let root_link_path = dir.path().join("root_link");
            unix::fs::symlink(&dir.path().join("root"), &root_link_path).unwrap();

            let tree = app.add_model(|ctx| Worktree::new(root_link_path, ctx));
            assert_condition(1, 300, || app.read(|ctx| tree.read(ctx).file_count() == 4)).await;
            app.read(|ctx| {
                let tree = tree.read(ctx);
                let results = match_paths(
                    Some(tree.snapshot()).iter(),
                    "bna",
                    false,
                    false,
                    false,
                    10,
                    ctx.thread_pool().clone(),
                )
                .iter()
                .map(|result| tree.path_for_inode(result.entry_id, true))
                .collect::<Result<Vec<PathBuf>, _>>()
                .unwrap();
                assert_eq!(
                    results,
                    vec![
                        PathBuf::from("root_link/banana/carrot/date"),
                        PathBuf::from("root_link/banana/carrot/endive"),
                    ]
                );
            })
        });
    }

    #[test]
    fn test_save_file() {
        App::test_async((), |mut app| async move {
            let dir = temp_tree(json!({
                "file1": "the old contents",
            }));

            let tree = app.add_model(|ctx| Worktree::new(dir.path(), ctx));
            assert_condition(1, 300, || app.read(|ctx| tree.read(ctx).file_count() == 1)).await;

            let buffer = Buffer::new(1, "a line of text.\n".repeat(10 * 1024));

            let file_inode = app.read(|ctx| {
                let tree = tree.read(ctx);
                let inode = tree.files().next().unwrap();
                assert_eq!(
                    tree.path_for_inode(inode, false)
                        .unwrap()
                        .file_name()
                        .unwrap(),
                    "file1"
                );
                inode
            });

            tree.update(&mut app, |tree, ctx| {
                smol::block_on(tree.save(file_inode, buffer.snapshot(), ctx.as_ref())).unwrap()
            });

            let loaded_history = app
                .read(|ctx| tree.read(ctx).load_history(file_inode, ctx))
                .await
                .unwrap();
            assert_eq!(loaded_history.base_text.as_ref(), buffer.text());
        });
    }

    #[test]
    fn test_rescan() {
        App::test_async((), |mut app| async move {
            let dir2 = temp_tree(json!({
                "dir1": {
                    "dir3": {
                        "file": "contents",
                    }
                },
                "dir2": {
                }
            }));
            let dir = temp_tree(json!({
                "dir1": {
                    "dir3": {
                        "file": "contents",
                    }
                },
                "dir2": {
                }
            }));

            let tree = app.add_model(|ctx| Worktree::new(dir.path(), ctx));
            assert_condition(1, 300, || app.read(|ctx| tree.read(ctx).file_count() == 1)).await;

            let dir_inode = app.read(|ctx| {
                tree.read(ctx)
                    .snapshot()
                    .inode_for_path("dir1/dir3")
                    .unwrap()
            });
            app.read(|ctx| {
                let tree = tree.read(ctx);
                assert_eq!(
                    tree.path_for_inode(dir_inode, false)
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "dir1/dir3"
                );
            });

            std::fs::rename(dir2.path(), dir.path().join("foo")).unwrap();
            assert_condition(1, 300, || {
                app.read(|ctx| {
                    let tree = tree.read(ctx);
                    tree.path_for_inode(dir_inode, false)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        == "dir2/dir3"
                })
            })
            .await;
            app.read(|ctx| {
                let tree = tree.read(ctx);
                assert_eq!(tree.snapshot().inode_for_path("dir2/dir3"), Some(dir_inode));
            });
        });
    }
}

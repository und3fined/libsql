use std::collections::BTreeMap;
use std::io::{BufWriter, IoSlice, Write};
use std::num::NonZeroU64;
use std::ops::DerefMut;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use fst::MapBuilder;
use parking_lot::{Mutex, RwLock};
use tokio_stream::Stream;
use zerocopy::{AsBytes, FromZeroes};

use crate::io::buf::{ZeroCopyBoxIoBuf, ZeroCopyBuf};
use crate::io::file::FileExt;
use crate::segment::SegmentFlags;
use crate::segment::{frame_offset, page_offset, sealed::SealedSegment};
use crate::transaction::{Transaction, TxGuard};

use super::list::SegmentList;
use super::{Frame, FrameHeader, SegmentHeader};

use crate::error::Result;

pub struct CurrentSegment<F> {
    path: PathBuf,
    index: SegmentIndex,
    header: Mutex<SegmentHeader>,
    file: Arc<F>,
    /// Read lock count on this segment. Each begin_read increments the count of readers on the current
    /// lock
    read_locks: Arc<AtomicU64>,
    sealed: AtomicBool,
    tail: Arc<SegmentList<F>>,
}

impl<F> CurrentSegment<F> {
    /// Create a new segment from the given path and metadata. The file pointed to by path must not
    /// exist.
    pub fn create(
        segment_file: F,
        path: PathBuf,
        start_frame_no: NonZeroU64,
        db_size: u32,
        tail: Arc<SegmentList<F>>,
    ) -> Result<Self>
    where
        F: FileExt,
    {
        let mut header = SegmentHeader {
            start_frame_no: start_frame_no.get().into(),
            last_commited_frame_no: 0.into(),
            db_size: db_size.into(),
            index_offset: 0.into(),
            index_size: 0.into(),
            header_cheksum: 0.into(),
            flags: 0.into(),
        };

        header.recompute_checksum();

        segment_file.write_all_at(header.as_bytes(), 0)?;

        Ok(Self {
            path: path.to_path_buf(),
            index: SegmentIndex::new(start_frame_no.get()),
            header: Mutex::new(header),
            file: segment_file.into(),
            read_locks: Arc::new(AtomicU64::new(0)),
            sealed: AtomicBool::default(),
            tail,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.count_committed() == 0
    }

    pub fn with_header<R>(&self, f: impl FnOnce(&SegmentHeader) -> R) -> R {
        let header = self.header.lock();
        f(&header)
    }

    pub fn last_committed(&self) -> u64 {
        self.header.lock().last_committed()
    }

    pub fn next_frame_no(&self) -> NonZeroU64 {
        self.header.lock().next_frame_no()
    }

    pub fn count_committed(&self) -> usize {
        self.header.lock().count_committed()
    }

    pub fn db_size(&self) -> u32 {
        self.header.lock().db_size.get()
    }

    /// insert a bunch of frames in the Wal. The frames needn't be ordered, therefore, on commit
    /// the last frame no needs to be passed alongside the new size_after.
    #[tracing::instrument(skip_all)]
    pub async fn insert_frames(
        &self,
        frames: Vec<Box<Frame>>,
        // (size_after, last_frame_no)
        commit_data: Option<(u32, u64)>,
        tx: &mut TxGuard<'_, F>,
    ) -> Result<Vec<Box<Frame>>>
    where
        F: FileExt,
    {
        assert!(!self.sealed.load(Ordering::SeqCst));
        {
            let tx = tx.deref_mut();
            // let mut commit_frame_written = false;
            let current_savepoint = tx.savepoints.last_mut().expect("no savepoints initialized");
            let mut frames = frame_list_to_option(frames);
            for i in 0..frames.len() {
                let offset = tx.next_offset;
                let buf = ZeroCopyBoxIoBuf(frames[i].take().unwrap());
                let (buf, ret) = self
                    .file
                    .write_all_at_async(buf, frame_offset(offset))
                    .await;

                ret?;

                let frame = buf.0;

                current_savepoint
                    .index
                    .insert(frame.header().page_no(), offset);
                tx.next_offset += 1;
                frames[i] = Some(frame);
            }

            if let Some((size_after, last_frame_no)) = commit_data {
                if tx.not_empty() {
                    let mut header = { *self.header.lock() };
                    header.last_commited_frame_no = last_frame_no.into();
                    header.db_size = size_after.into();
                    // set frames unordered because there are no guarantees that we received frames
                    // in order.
                    header.set_flags(header.flags().union(SegmentFlags::FRAME_UNORDERED));
                    header.recompute_checksum();

                    let (header, ret) = self
                        .file
                        .write_all_at_async(ZeroCopyBuf::new_init(header), 0)
                        .await;

                    ret?;

                    // self.file.sync_data().unwrap();
                    tx.merge_savepoints(&mut self.index.index.write());
                    // set the header last, so that a transaction does not witness a write before
                    // it's actually committed.
                    *self.header.lock() = header.into_inner();

                    tx.is_commited = true;
                }
            }

            let frames = options_to_frame_list(frames);

            Ok(frames)
        }
    }

    #[tracing::instrument(skip(self, pages, tx))]
    pub fn insert_pages<'a>(
        &self,
        pages: impl Iterator<Item = (u32, &'a [u8])>,
        size_after: Option<u32>,
        tx: &mut TxGuard<F>,
    ) -> Result<Option<u64>>
    where
        F: FileExt,
    {
        assert!(!self.sealed.load(Ordering::SeqCst));
        {
            let tx = tx.deref_mut();
            let mut pages = pages.peekable();
            // let mut commit_frame_written = false;
            let current_savepoint = tx.savepoints.last_mut().expect("no savepoints initialized");
            while let Some((page_no, page)) = pages.next() {
                // optim: if the page is already present, overwrite its content
                if let Some(offset) = current_savepoint.index.get(&page_no) {
                    tracing::trace!(page_no, "recycling frame");
                    self.file.write_all_at(page, page_offset(*offset))?;
                    continue;
                }

                tracing::trace!(page_no, "inserting new frame");
                let size_after = if let Some(size) = size_after {
                    pages.peek().is_none().then_some(size).unwrap_or(0)
                } else {
                    0
                };

                let frame_no = tx.next_frame_no;
                let header = FrameHeader {
                    page_no: page_no.into(),
                    size_after: size_after.into(),
                    frame_no: frame_no.into(),
                };
                let slices = &[IoSlice::new(header.as_bytes()), IoSlice::new(&page)];
                let offset = tx.next_offset;
                debug_assert_eq!(
                    self.header.lock().start_frame_no.get() + offset as u64,
                    frame_no
                );
                self.file.write_at_vectored(slices, frame_offset(offset))?;
                assert!(
                    current_savepoint.index.insert(page_no, offset).is_none(),
                    "existing frames should be recycled"
                );
                tx.next_frame_no += 1;
                tx.next_offset += 1;
            }
        }

        if let Some(size_after) = size_after {
            if tx.not_empty() {
                let last_frame_no = tx.next_frame_no - 1;
                let mut header = { *self.header.lock() };
                header.last_commited_frame_no = last_frame_no.into();
                header.db_size = size_after.into();
                header.recompute_checksum();

                self.file.write_all_at(header.as_bytes(), 0)?;
                // self.file.sync_data().unwrap();
                tx.merge_savepoints(&mut self.index.index.write());
                // set the header last, so that a transaction does not witness a write before
                // it's actually committed.
                *self.header.lock() = header;

                tx.is_commited = true;

                return Ok(Some(last_frame_no));
            }
        }
        Ok(None)
    }

    /// return the offset of the frame for page_no, with frame_no no larger that max_frame_no, if
    /// it exists
    pub fn find_frame(&self, page_no: u32, tx: &Transaction<F>) -> Option<u32> {
        // if it's a write transaction, check its transient index first
        if let Transaction::Write(ref tx) = tx {
            if let Some(offset) = tx.find_frame_offset(page_no) {
                return Some(offset);
            }
        }

        // not a write tx, or page is not in write tx, look into the segment
        self.index.locate(page_no, tx.max_frame_no)
    }

    /// reads the page conainted in frame at offset into buf
    #[tracing::instrument(skip(self, buf))]
    pub fn read_page_offset(&self, offset: u32, buf: &mut [u8]) -> Result<()>
    where
        F: FileExt,
    {
        tracing::trace!("read page");
        debug_assert_eq!(buf.len(), 4096);
        self.file.read_exact_at(buf, page_offset(offset))?;

        Ok(())
    }

    #[allow(dead_code)]
    pub fn frame_header_at(&self, offset: u32) -> Result<FrameHeader>
    where
        F: FileExt,
    {
        let mut header = FrameHeader::new_zeroed();
        self.file
            .read_exact_at(header.as_bytes_mut(), frame_offset(offset))?;
        Ok(header)
    }

    /// It is expected that sealing is performed under a write lock
    #[tracing::instrument(skip_all)]
    pub fn seal(&self) -> Result<Option<SealedSegment<F>>>
    where
        F: FileExt,
    {
        let mut header = self.header.lock();
        let index_offset = header.count_committed() as u32;
        let index_byte_offset = frame_offset(index_offset);
        let mut cursor = self.file.cursor(index_byte_offset);
        let mut writer = BufWriter::new(&mut cursor);
        self.index.merge_all(&mut writer)?;
        writer.into_inner().map_err(|e| e.into_parts().0)?;
        header.index_offset = index_byte_offset.into();
        header.index_size = cursor.count().into();
        header.recompute_checksum();
        self.file.write_all_at(header.as_bytes(), 0)?;
        let sealed = SealedSegment::open(
            self.file.clone(),
            self.path.clone(),
            self.read_locks.clone(),
        )?;

        // we only flip the sealed mark when no more error can occur, or we risk to deadlock a read
        // transaction waiting for a more recent version of the segment that is never going to arrive
        assert!(
            self.sealed
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok(),
            "attempt to seal an already sealed segment"
        );

        tracing::debug!("segment sealed");

        Ok(sealed)
    }

    pub fn last_committed_frame_no(&self) -> u64 {
        let header = self.header.lock();
        if header.last_commited_frame_no.get() == 0 {
            header.start_frame_no.get()
        } else {
            header.last_commited_frame_no.get()
        }
    }

    pub fn inc_reader_count(&self) {
        self.read_locks().fetch_add(1, Ordering::SeqCst);
    }

    pub fn dec_reader_count(&self) {
        self.read_locks().fetch_sub(1, Ordering::SeqCst);
    }

    pub fn read_locks(&self) -> &AtomicU64 {
        self.read_locks.as_ref()
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::SeqCst)
    }

    pub fn tail(&self) -> &Arc<SegmentList<F>> {
        &self.tail
    }

    // todo: maybe return boxed frames?
    pub fn rev_frame_stream(&self) -> impl Stream<Item = Result<Frame>> + '_
    where
        F: FileExt,
    {
        async_stream::try_stream! {
            let (start_frame_no, last_committed) = {
                let header = self.header.lock();
                (header.start_frame_no.get(), header.last_commited_frame_no.get())
            };
            let mut next_offset = (last_committed - start_frame_no) as u32;
            loop {
                let byte_offset = frame_offset(next_offset);

                let buf = ZeroCopyBuf::<Frame>::new_uninit();
                let (buf, ret) = self.file.read_exact_at_async(buf, byte_offset).await;
                ret?;
                yield buf.into_inner();
                if next_offset == 0 {
                    break
                } else {
                    next_offset -= 1;
                }
            }
        }
    }
}

fn frame_list_to_option(frames: Vec<Box<Frame>>) -> Vec<Option<Box<Frame>>> {
    // this is safe because Option<Box<T>> and Box<T> are the same size and Frame is sized:
    // https://doc.rust-lang.org/std/option/index.html#representation
    unsafe { std::mem::transmute(frames) }
}

fn options_to_frame_list(frames: Vec<Option<Box<Frame>>>) -> Vec<Box<Frame>> {
    debug_assert!(frames.iter().all(|f| f.is_some()));
    // this is safe because Option<Box<T>> and Box<T> are the same size and Frame is sized:
    // https://doc.rust-lang.org/std/option/index.html#representation
    unsafe { std::mem::transmute(frames) }
}

impl<F> Drop for CurrentSegment<F> {
    fn drop(&mut self) {
        // todo: if reader is 0 and segment is sealed, register for compaction.
    }
}

/// TODO: implement spill-to-disk when txn is too large
struct SegmentIndex {
    start_frame_no: u64,
    index: RwLock<BTreeMap<u32, Vec<u32>>>,
}

impl SegmentIndex {
    pub fn new(start_frame_no: u64) -> Self {
        Self {
            start_frame_no,
            index: Default::default(),
        }
    }
    fn locate(&self, page_no: u32, max_frame_no: u64) -> Option<u32> {
        let index = self.index.read();
        let offsets = index.get(&page_no)?;
        offsets
            .iter()
            .rev()
            .find(|fno| self.start_frame_no + **fno as u64 <= max_frame_no)
            .copied()
    }

    #[tracing::instrument(skip_all)]
    fn merge_all<W: Write>(&self, writer: W) -> Result<()> {
        let index = self.index.read();
        let mut builder = MapBuilder::new(writer)?;
        for (key, entries) in index.iter() {
            let offset = *entries.last().unwrap();
            builder.insert(key.to_be_bytes(), offset as u64)?;
        }

        builder.finish()?;
        Ok(())
    }
}

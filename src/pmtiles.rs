// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! A minimal, bounded PMTiles v3 reader: just enough to fetch the vector
//! tiles of the same self-hosted map archive the Web client draws
//! (`/map/0.1.0/basemap.pmtiles`). A text client has no MapLibre; Core reads
//! the tiles' `roads` (`nilxone_contracts::vector_tile`), this module only
//! finds and decompresses them. It mirrors the `pmtiles` npm reader's
//! directory lookup and tile ids, and fails closed on anything unexpected.

use std::future::Future;
use std::io::Read as _;

use flate2::read::GzDecoder;
use reqwest::{Client, StatusCode, header::RANGE, redirect::Policy};
use url::Url;

const HEADER_BYTES: usize = 127;
/// The reference reader fetches this much first: header plus root directory.
const ROOT_FETCH_BYTES: u64 = 16_384;
/// Leaf directories and tiles are small; a larger range is a broken archive.
const MAX_RANGE_BYTES: u64 = 4 * 1024 * 1024;
/// Decompressed size cap, so one tile cannot inflate without bound.
const MAX_INFLATED_BYTES: u64 = 16 * 1024 * 1024;
/// The specification allows at most three levels below the root.
const MAX_DIRECTORY_DEPTH: usize = 4;
/// Deepest zoom whose tile ids stay below 2^53 in the reference reader.
const MAX_ZOOM: u8 = 26;

const COMPRESSION_NONE: u8 = 1;
const COMPRESSION_GZIP: u8 = 2;
const TILE_TYPE_MVT: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveError {
    /// The archive could not be reached, or answered without the range.
    Unavailable,
    /// Not a PMTiles v3 vector archive this reader supports.
    Unsupported,
    /// Truncated, oversized or inconsistent bytes.
    Malformed,
    /// The tile address is outside its zoom.
    InvalidTile,
}

/// Where archive bytes come from: HTTP range requests in production, memory
/// in tests.
pub trait RangeSource {
    fn read(
        &self,
        offset: u64,
        length: u64,
    ) -> impl Future<Output = Result<Vec<u8>, ArchiveError>> + Send;
}

/// The archive over HTTP `Range` requests. Redirects are refused: the
/// configured URL is the archive.
#[derive(Clone)]
pub struct HttpRangeSource {
    http: Client,
    url: Url,
}

impl HttpRangeSource {
    pub fn new(url: &str) -> Result<Self, ArchiveError> {
        let url = Url::parse(url).map_err(|_| ArchiveError::Unsupported)?;
        let local = url
            .host_str()
            .is_some_and(|host| host == "localhost" || host == "127.0.0.1");
        if url.scheme() != "https" && !(url.scheme() == "http" && local) {
            return Err(ArchiveError::Unsupported);
        }
        let http = Client::builder()
            .redirect(Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|_| ArchiveError::Unavailable)?;
        Ok(Self { http, url })
    }
}

impl RangeSource for HttpRangeSource {
    async fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, ArchiveError> {
        if length == 0 || length > MAX_RANGE_BYTES {
            return Err(ArchiveError::Malformed);
        }
        let last = offset
            .checked_add(length - 1)
            .ok_or(ArchiveError::Malformed)?;
        let response = self
            .http
            .get(self.url.clone())
            .header(RANGE, format!("bytes={offset}-{last}"))
            .send()
            .await
            .map_err(|_| ArchiveError::Unavailable)?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(ArchiveError::Unavailable);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| ArchiveError::Unavailable)?;
        // The root fetch may run past a small archive's end; nothing else may
        // come back short or long.
        if bytes.len() as u64 > length || (offset != 0 && bytes.len() as u64 != length) {
            return Err(ArchiveError::Malformed);
        }
        Ok(bytes.to_vec())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Header {
    root_offset: u64,
    root_length: u64,
    leaf_offset: u64,
    tile_data_offset: u64,
    internal_compression: u8,
    tile_compression: u8,
    min_zoom: u8,
    max_zoom: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Entry {
    tile_id: u64,
    offset: u64,
    length: u64,
    run_length: u64,
}

/// An opened archive: its header and root directory, read once.
pub struct MapArchive<S> {
    source: S,
    header: Header,
    root: Vec<Entry>,
}

impl<S: RangeSource + Sync> MapArchive<S> {
    /// Reads the header and the root directory.
    pub async fn open(source: S) -> Result<Self, ArchiveError> {
        let start = source.read(0, ROOT_FETCH_BYTES).await?;
        let header = parse_header(&start)?;
        let root_end = header
            .root_offset
            .checked_add(header.root_length)
            .ok_or(ArchiveError::Malformed)?;
        let root_bytes = if root_end <= start.len() as u64 {
            start[header.root_offset as usize..root_end as usize].to_vec()
        } else {
            source.read(header.root_offset, header.root_length).await?
        };
        let root = parse_directory(&inflate(&root_bytes, header.internal_compression)?)?;
        Ok(Self {
            source,
            header,
            root,
        })
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> &S {
        &self.source
    }

    /// The decompressed vector tile at `z`/`x`/`y`, or `None` where the
    /// archive holds no tile (open sea, outside its extract).
    pub async fn tile(&self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>, ArchiveError> {
        let tile_id = tile_id(z, x, y)?;
        if z < self.header.min_zoom || z > self.header.max_zoom {
            return Ok(None);
        }
        let mut leaf: Option<Vec<Entry>> = None;
        for _ in 0..MAX_DIRECTORY_DEPTH {
            let directory = leaf.as_deref().unwrap_or(&self.root);
            let Some(entry) = find_tile(directory, tile_id) else {
                return Ok(None);
            };
            if entry.run_length > 0 {
                let offset = self
                    .header
                    .tile_data_offset
                    .checked_add(entry.offset)
                    .ok_or(ArchiveError::Malformed)?;
                let bytes = self.source.read(offset, entry.length).await?;
                return inflate(&bytes, self.header.tile_compression).map(Some);
            }
            let offset = self
                .header
                .leaf_offset
                .checked_add(entry.offset)
                .ok_or(ArchiveError::Malformed)?;
            let bytes = self.source.read(offset, entry.length).await?;
            let entries = parse_directory(&inflate(&bytes, self.header.internal_compression)?)?;
            if entries.is_empty() {
                return Err(ArchiveError::Malformed);
            }
            leaf = Some(entries);
        }
        Err(ArchiveError::Malformed)
    }
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

fn parse_header(bytes: &[u8]) -> Result<Header, ArchiveError> {
    if bytes.len() < HEADER_BYTES || &bytes[..7] != b"PMTiles" {
        return Err(ArchiveError::Unsupported);
    }
    if bytes[7] != 3 || bytes[99] != TILE_TYPE_MVT {
        return Err(ArchiveError::Unsupported);
    }
    for compression in [bytes[97], bytes[98]] {
        if compression != COMPRESSION_NONE && compression != COMPRESSION_GZIP {
            return Err(ArchiveError::Unsupported);
        }
    }
    Ok(Header {
        root_offset: u64_at(bytes, 8),
        root_length: u64_at(bytes, 16),
        leaf_offset: u64_at(bytes, 40),
        tile_data_offset: u64_at(bytes, 56),
        internal_compression: bytes[97],
        tile_compression: bytes[98],
        min_zoom: bytes[100],
        max_zoom: bytes[101],
    })
}

fn inflate(bytes: &[u8], compression: u8) -> Result<Vec<u8>, ArchiveError> {
    match compression {
        COMPRESSION_NONE => Ok(bytes.to_vec()),
        COMPRESSION_GZIP => {
            let mut out = Vec::new();
            GzDecoder::new(bytes)
                .take(MAX_INFLATED_BYTES + 1)
                .read_to_end(&mut out)
                .map_err(|_| ArchiveError::Malformed)?;
            if out.len() as u64 > MAX_INFLATED_BYTES {
                return Err(ArchiveError::Malformed);
            }
            Ok(out)
        }
        _ => Err(ArchiveError::Unsupported),
    }
}

fn varint(bytes: &[u8], position: &mut usize) -> Result<u64, ArchiveError> {
    let mut result = 0u64;
    for shift in (0..64).step_by(7) {
        let &byte = bytes.get(*position).ok_or(ArchiveError::Malformed)?;
        *position += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(result);
        }
    }
    Err(ArchiveError::Malformed)
}

fn parse_directory(bytes: &[u8]) -> Result<Vec<Entry>, ArchiveError> {
    let mut at = 0;
    let count = varint(bytes, &mut at)?;
    // Every entry takes at least four bytes, so the count is bounded by them.
    if count > bytes.len() as u64 {
        return Err(ArchiveError::Malformed);
    }
    let count = count as usize;
    let mut entries = Vec::with_capacity(count);
    let mut tile_id = 0u64;
    for _ in 0..count {
        tile_id = tile_id
            .checked_add(varint(bytes, &mut at)?)
            .ok_or(ArchiveError::Malformed)?;
        entries.push(Entry {
            tile_id,
            offset: 0,
            length: 0,
            run_length: 1,
        });
    }
    for entry in &mut entries {
        entry.run_length = varint(bytes, &mut at)?;
    }
    for entry in &mut entries {
        entry.length = varint(bytes, &mut at)?;
    }
    for index in 0..count {
        let value = varint(bytes, &mut at)?;
        entries[index].offset = if value == 0 && index > 0 {
            entries[index - 1]
                .offset
                .checked_add(entries[index - 1].length)
                .ok_or(ArchiveError::Malformed)?
        } else {
            value.checked_sub(1).ok_or(ArchiveError::Malformed)?
        };
    }
    if at != bytes.len() {
        return Err(ArchiveError::Malformed);
    }
    Ok(entries)
}

/// The entry holding `tile_id`: exact, or the run or leaf directory before it.
fn find_tile(entries: &[Entry], tile_id: u64) -> Option<Entry> {
    match entries.binary_search_by_key(&tile_id, |entry| entry.tile_id) {
        Ok(at) => Some(entries[at]),
        Err(0) => None,
        Err(after) => {
            let entry = entries[after - 1];
            (entry.run_length == 0 || tile_id - entry.tile_id < entry.run_length).then_some(entry)
        }
    }
}

/// `(nx, ny)` after one Hilbert step, as the reference `rotate`. Like the
/// reference, coordinates may go negative between steps; only their low bits
/// are read again.
const fn rotate(n: i64, x: i64, y: i64, rx: i64, ry: i64) -> (i64, i64) {
    if ry != 0 {
        (x, y)
    } else if rx != 0 {
        (n - 1 - y, n - 1 - x)
    } else {
        (y, x)
    }
}

/// The archive's tile id: the tiles of every shallower zoom, then the
/// Hilbert index within zoom `z`, as the reference `zxyToTileId`.
pub fn tile_id(z: u8, x: u32, y: u32) -> Result<u64, ArchiveError> {
    if z > MAX_ZOOM || u64::from(x) >= 1 << z || u64::from(y) >= 1 << z {
        return Err(ArchiveError::InvalidTile);
    }
    let mut id = ((1i64 << z) * (1i64 << z) - 1) / 3;
    let (mut x, mut y) = (i64::from(x), i64::from(y));
    let mut a = if z == 0 { 0 } else { 1i64 << (z - 1) };
    while a > 0 {
        let rx = x & a;
        let ry = y & a;
        id += ((3 * rx) ^ ry) * a;
        (x, y) = rotate(a, x, y, rx, ry);
        a >>= 1;
    }
    u64::try_from(id).map_err(|_| ArchiveError::InvalidTile)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write as _;

    pub(crate) struct MemorySource(pub Vec<u8>);

    impl RangeSource for MemorySource {
        async fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, ArchiveError> {
            let start = usize::try_from(offset).map_err(|_| ArchiveError::Malformed)?;
            if start > self.0.len() {
                return Err(ArchiveError::Malformed);
            }
            let end = start.saturating_add(length as usize).min(self.0.len());
            Ok(self.0[start..end].to_vec())
        }
    }

    pub(crate) fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn put_varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn directory(entries: &[Entry]) -> Vec<u8> {
        let mut out = Vec::new();
        put_varint(&mut out, entries.len() as u64);
        let mut last = 0;
        for entry in entries {
            put_varint(&mut out, entry.tile_id - last);
            last = entry.tile_id;
        }
        for entry in entries {
            put_varint(&mut out, entry.run_length);
        }
        for entry in entries {
            put_varint(&mut out, entry.length);
        }
        for entry in entries {
            put_varint(&mut out, entry.offset + 1);
        }
        out
    }

    /// An archive of gzip MVT tiles keyed by `(z, x, y)`. With `leaf`, the
    /// root points at one leaf directory holding every tile.
    /// A tile's `(z, x, y)` and its uncompressed bytes.
    pub(crate) type FixtureTile = ((u8, u32, u32), Vec<u8>);

    pub(crate) fn archive(tiles: &[FixtureTile], leaf: bool) -> Vec<u8> {
        let mut sorted: Vec<(u64, Vec<u8>)> = tiles
            .iter()
            .map(|((z, x, y), bytes)| (tile_id(*z, *x, *y).unwrap(), gzip(bytes)))
            .collect();
        sorted.sort_by_key(|(id, _)| *id);
        let mut data = Vec::new();
        let mut entries = Vec::new();
        for (id, bytes) in &sorted {
            entries.push(Entry {
                tile_id: *id,
                offset: data.len() as u64,
                length: bytes.len() as u64,
                run_length: 1,
            });
            data.extend(bytes);
        }
        let (root, leaves) = if leaf {
            let leaves = gzip(&directory(&entries));
            let root = gzip(&directory(&[Entry {
                tile_id: entries[0].tile_id,
                offset: 0,
                length: leaves.len() as u64,
                run_length: 0,
            }]));
            (root, leaves)
        } else {
            (gzip(&directory(&entries)), Vec::new())
        };
        let mut header = vec![0u8; HEADER_BYTES];
        header[..7].copy_from_slice(b"PMTiles");
        header[7] = 3;
        let root_offset = HEADER_BYTES as u64;
        let leaf_offset = root_offset + root.len() as u64;
        let data_offset = leaf_offset + leaves.len() as u64;
        for (at, value) in [
            (8, root_offset),
            (16, root.len() as u64),
            (40, leaf_offset),
            (48, leaves.len() as u64),
            (56, data_offset),
            (64, data.len() as u64),
        ] {
            header[at..at + 8].copy_from_slice(&value.to_le_bytes());
        }
        header[97] = COMPRESSION_GZIP;
        header[98] = COMPRESSION_GZIP;
        header[99] = TILE_TYPE_MVT;
        header[100] = 0;
        header[101] = 15;
        [header, root, leaves, data].concat()
    }

    #[test]
    fn tile_ids_match_the_reference_reader() {
        // Values from pmtiles@4.5.0 `zxyToTileId`.
        for ((z, x, y), id) in [
            ((0, 0, 0), 0),
            ((1, 0, 0), 1),
            ((1, 0, 1), 2),
            ((1, 1, 1), 3),
            ((1, 1, 0), 4),
            ((2, 0, 0), 5),
            ((3, 5, 2), 76),
            ((14, 9581, 5528), 316_527_214),
            ((15, 30147, 19659), 1_152_841_269),
            ((20, 340_000, 640_000), 886_021_904_725),
            ((26, 67_108_863, 67_108_863), 4_503_599_627_370_495),
        ] {
            assert_eq!(tile_id(z, x, y), Ok(id), "{z}/{x}/{y}");
        }
        assert_eq!(tile_id(1, 2, 0), Err(ArchiveError::InvalidTile));
        assert_eq!(tile_id(27, 0, 0), Err(ArchiveError::InvalidTile));
    }

    #[tokio::test]
    async fn reads_tiles_through_root_and_leaf_directories() {
        let tiles = vec![
            ((14, 9581, 5528), b"kyiv".to_vec()),
            ((14, 9582, 5528), b"east".to_vec()),
            ((0, 0, 0), b"world".to_vec()),
        ];
        for leaf in [false, true] {
            let archive = MapArchive::open(MemorySource(archive(&tiles, leaf)))
                .await
                .unwrap();
            assert_eq!(
                archive.tile(14, 9581, 5528).await.unwrap().unwrap(),
                b"kyiv"
            );
            assert_eq!(
                archive.tile(14, 9582, 5528).await.unwrap().unwrap(),
                b"east"
            );
            assert_eq!(archive.tile(0, 0, 0).await.unwrap().unwrap(), b"world");
            assert_eq!(archive.tile(14, 9583, 5528).await.unwrap(), None);
            assert_eq!(archive.tile(16, 0, 0).await.unwrap(), None);
        }
    }

    #[tokio::test]
    async fn refuses_what_it_cannot_read() {
        let good = archive(&[((14, 9581, 5528), b"kyiv".to_vec())], false);
        let mut wrong_magic = good.clone();
        wrong_magic[0] = b'X';
        let mut raster = good.clone();
        raster[99] = 2;
        let mut brotli = good.clone();
        brotli[98] = 3;
        for bytes in [wrong_magic, raster, brotli, good[..100].to_vec()] {
            assert_eq!(
                MapArchive::open(MemorySource(bytes)).await.err(),
                Some(ArchiveError::Unsupported)
            );
        }
        let mut truncated_root = good.clone();
        truncated_root.truncate(HEADER_BYTES + 3);
        assert_eq!(
            MapArchive::open(MemorySource(truncated_root)).await.err(),
            Some(ArchiveError::Malformed)
        );
    }

    #[test]
    fn a_gzip_bomb_stops_at_the_cap() {
        let bomb = gzip(&vec![0u8; MAX_INFLATED_BYTES as usize + 1]);
        assert_eq!(
            inflate(&bomb, COMPRESSION_GZIP),
            Err(ArchiveError::Malformed)
        );
    }

    #[test]
    fn only_https_or_a_local_http_archive() {
        assert!(HttpRangeSource::new("https://nilx.one/map/0.1.0/basemap.pmtiles").is_ok());
        assert!(HttpRangeSource::new("http://localhost:8080/basemap.pmtiles").is_ok());
        assert!(HttpRangeSource::new("http://nilx.one/map/0.1.0/basemap.pmtiles").is_err());
        assert!(HttpRangeSource::new("file:///basemap.pmtiles").is_err());
    }
}

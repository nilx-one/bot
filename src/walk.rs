// © 2026 aiaiaiai · aiaiaiai.org
// SPDX-License-Identifier: MPL-2.0

//! Whether an Avaia can walk to a paid Special's pickup point, for a text
//! client with no map renderer. The bot reads the `roads` of the same map
//! archive the Web client draws and asks Core for the walking graph and the
//! route (`nilxone_contracts::{vector_tile, walk_graph}`); Core alone decides
//! which pickup choices to offer (`special_pickup_choices`).
//!
//! Presentation only: an answer never dispatches xPing, starts a walk, moves
//! an Avaia or proves payment or presence. `paid`, the Avaia's in-world
//! position and the point must come from a verified Core Gateway response,
//! which does not exist yet, so nothing here is wired into the bot.

#![allow(dead_code)]

use nilxone_contracts::vector_tile::{
    MIN_WALK_TILE_ZOOM, RoadTile, TileId, build_walk_graph_from_tiles,
};
use nilxone_contracts::walk_graph::{
    LonLat, SNAP_DISTANCE_M, WalkGraph, reach_from, snap_candidates,
};
use nilxone_contracts::{
    AVAIA_SPECIAL_PICKUP_WALK_RADIUS_METERS, GEO_COORDINATE_E7_SCALE, GeoCoordinate,
    SpecialPickupChoices, SpecialPickupRequest, distance_meters, special_pickup_choices,
};

use crate::pmtiles::{ArchiveError, HttpRangeSource, MapArchive, RangeSource};

/// The zoom roads are read at: Core's minimum, and Web's `ROAD_TILE_ZOOM`.
pub const WALK_TILE_ZOOM: u8 = MIN_WALK_TILE_ZOOM;

/// The most tiles one answer reads. Within the walking radius plus its margin
/// it is at most four; more means bad coordinates.
const MAX_WALK_TILES: usize = 16;

/// How far beyond both ends the network is read, so a route may detour.
const ROUTE_MARGIN_M: f64 = AVAIA_SPECIAL_PICKUP_WALK_RADIUS_METERS as f64;

const METERS_PER_DEGREE_LATITUDE: f64 = 111_320.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkError {
    Archive(ArchiveError),
    /// A Core vector tile error code (`malformed`, `compressed`, …).
    Tile(&'static str),
    /// The ends are too far apart for a bounded read.
    TooManyTiles,
}

impl From<ArchiveError> for WalkError {
    fn from(error: ArchiveError) -> Self {
        Self::Archive(error)
    }
}

/// The map archive named by `BOT_MAP_ARCHIVE_URL`, e.g.
/// `https://nilx.one/map/0.1.0/basemap.pmtiles`; `None` when unset.
pub fn archive_source_from_env() -> Result<Option<HttpRangeSource>, ArchiveError> {
    match std::env::var("BOT_MAP_ARCHIVE_URL") {
        Ok(url) if !url.trim().is_empty() => HttpRangeSource::new(url.trim()).map(Some),
        _ => Ok(None),
    }
}

/// Core's pickup choices, with `walk_route_available` found on the archive's
/// roads. The archive is read only for a paid item whose Avaia stands inside
/// the walking radius; any failure to read it fails closed (no walk), and
/// xPing stays offered as Core decides.
pub async fn pickup_choices<S: RangeSource + Sync>(
    archive: Option<&MapArchive<S>>,
    paid: bool,
    point: GeoCoordinate,
    avaia: Option<GeoCoordinate>,
) -> SpecialPickupChoices {
    let walk_route_available = match (archive, avaia) {
        (Some(archive), Some(avaia))
            if paid && distance_meters(avaia, point) < AVAIA_SPECIAL_PICKUP_WALK_RADIUS_METERS =>
        {
            walk_route_available(archive, avaia, point)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(?error, "walking route unavailable; not suggesting a walk");
                    false
                })
        }
        _ => false,
    };
    special_pickup_choices(&SpecialPickupRequest {
        paid,
        point,
        avaia,
        walk_route_available,
    })
}

/// Whether the archive's roads join `from` to `to` on foot, from somewhere
/// within Core's snap distance of one end to somewhere within it of the other.
/// The nearest edge alone is not enough: in a real archive it can be a short
/// stub whose links Core does not walk, a few metres from the connected
/// network.
pub async fn walk_route_available<S: RangeSource + Sync>(
    archive: &MapArchive<S>,
    from: GeoCoordinate,
    to: GeoCoordinate,
) -> Result<bool, WalkError> {
    let (from, to) = (lon_lat(from), lon_lat(to));
    let mut fetched = Vec::new();
    for tile in covering_tiles(from, to)? {
        if let Some(bytes) = archive.tile(tile.z, tile.x, tile.y).await? {
            fetched.push((tile, bytes));
        }
    }
    let tiles: Vec<RoadTile<'_>> = fetched
        .iter()
        .map(|(tile, bytes)| RoadTile { tile: *tile, bytes })
        .collect();
    let graph =
        build_walk_graph_from_tiles(&tiles).map_err(|error| WalkError::Tile(error.code()))?;
    Ok(joined(&graph, from, to))
}

/// The most snap candidates tried at each end; nearest first.
const MAX_SNAP_CANDIDATES: usize = 8;

fn joined(graph: &WalkGraph, from: LonLat, to: LonLat) -> bool {
    let ends: Vec<[usize; 2]> = snap_candidates(graph, to, SNAP_DISTANCE_M)
        .into_iter()
        .take(MAX_SNAP_CANDIDATES)
        .map(|end| {
            let edge = graph.edges[end.edge];
            [edge.a, edge.b]
        })
        .collect();
    if ends.is_empty() {
        return false;
    }
    snap_candidates(graph, from, SNAP_DISTANCE_M)
        .into_iter()
        .take(MAX_SNAP_CANDIDATES)
        .any(|start| {
            // A start on the same edge as an end reaches that edge's nodes.
            let reach = reach_from(graph, &start, f64::INFINITY, None);
            ends.iter()
                .flatten()
                .any(|&node| reach.cost.get(node).is_some_and(|cost| cost.is_finite()))
        })
}

fn lon_lat(at: GeoCoordinate) -> LonLat {
    let scale = f64::from(GEO_COORDINATE_E7_SCALE);
    [
        f64::from(at.longitude_e7()) / scale,
        f64::from(at.latitude_e7()) / scale,
    ]
}

/// The z14 tiles under the box around both ends, padded by the margin.
fn covering_tiles(a: LonLat, b: LonLat) -> Result<Vec<TileId>, WalkError> {
    let pad_lat = ROUTE_MARGIN_M / METERS_PER_DEGREE_LATITUDE;
    let middle = (a[1] + b[1]) / 2.0;
    let pad_lon = pad_lat / middle.to_radians().cos().max(0.01);
    let west = a[0].min(b[0]) - pad_lon;
    let east = a[0].max(b[0]) + pad_lon;
    let south = (a[1].min(b[1]) - pad_lat).max(-85.0);
    let north = (a[1].max(b[1]) + pad_lat).min(85.0);
    let (x0, y0) = tile_of(west, north);
    let (x1, y1) = tile_of(east, south);
    let count = (u64::from(x1 - x0) + 1) * (u64::from(y1 - y0) + 1);
    if count > MAX_WALK_TILES as u64 {
        return Err(WalkError::TooManyTiles);
    }
    let mut tiles = Vec::new();
    for x in x0..=x1 {
        for y in y0..=y1 {
            tiles.push(TileId {
                z: WALK_TILE_ZOOM,
                x,
                y,
            });
        }
    }
    Ok(tiles)
}

/// The z14 tile holding a point, clamped to the pyramid.
fn tile_of(longitude: f64, latitude: f64) -> (u32, u32) {
    let n = f64::from(1u32 << WALK_TILE_ZOOM);
    let x = ((longitude + 180.0) / 360.0 * n).floor();
    let latitude = latitude.to_radians();
    let y = ((1.0 - latitude.tan().asinh() / std::f64::consts::PI) / 2.0 * n).floor();
    let clamp = |v: f64| v.clamp(0.0, n - 1.0) as u32;
    (clamp(x), clamp(y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pmtiles::tests::{MemorySource, archive};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TILE: (u8, u32, u32) = (14, 9581, 5528);

    // A minimal vector tile writer for one `roads` layer of line features.
    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        while value >= 0x80 {
            out.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
        out
    }
    fn delimited(field: u64, bytes: &[u8]) -> Vec<u8> {
        [
            varint(field << 3 | 2),
            varint(bytes.len() as u64),
            bytes.to_vec(),
        ]
        .concat()
    }
    fn zz(n: i64) -> u64 {
        if n >= 0 {
            (n * 2) as u64
        } else {
            (-n * 2 - 1) as u64
        }
    }
    fn roads_tile(lines: &[[(i64, i64); 2]]) -> Vec<u8> {
        let mut layer = [varint(15 << 3), varint(2), delimited(1, b"roads")].concat();
        for [(ax, ay), (bx, by)] in lines {
            let geometry: Vec<u8> = [9, zz(*ax), zz(*ay), 10, zz(bx - ax), zz(by - ay)]
                .into_iter()
                .flat_map(varint)
                .collect();
            // Tags: kind = path, kind_detail = footway.
            let tags = [varint(0), varint(0), varint(1), varint(1)].concat();
            let feature = [
                delimited(2, &tags),
                varint(3 << 3),
                varint(2),
                delimited(4, &geometry),
            ]
            .concat();
            layer.extend(delimited(2, &feature));
        }
        layer.extend(delimited(3, b"kind"));
        layer.extend(delimited(3, b"kind_detail"));
        layer.extend(delimited(4, &delimited(1, b"path")));
        layer.extend(delimited(4, &delimited(1, b"footway")));
        layer.extend([varint(5 << 3), varint(4096)].concat());
        delimited(3, &layer)
    }

    /// Where tile pixel `(px, py)` of the Kyiv tile lies.
    fn at(px: f64, py: f64) -> GeoCoordinate {
        let n = f64::from(1u32 << 14);
        let longitude = (f64::from(TILE.1) + px / 4096.0) / n * 360.0 - 180.0;
        let latitude = (std::f64::consts::PI * (1.0 - 2.0 * (f64::from(TILE.2) + py / 4096.0) / n))
            .sinh()
            .atan()
            .to_degrees();
        GeoCoordinate::from_degrees(longitude, latitude).unwrap()
    }

    async fn kyiv(lines: &[[(i64, i64); 2]]) -> MapArchive<MemorySource> {
        MapArchive::open(MemorySource(archive(&[(TILE, roads_tile(lines))], false)))
            .await
            .unwrap()
    }

    /// Counts reads, to prove a read was not needed.
    struct Counting(MemorySource, AtomicUsize);

    impl RangeSource for Counting {
        async fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, ArchiveError> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.read(offset, length).await
        }
    }

    #[tokio::test]
    async fn a_connected_footway_suggests_the_walk() {
        // About 150 m of footway, the ends on it.
        let archive = kyiv(&[[(1000, 1000), (1400, 1000)]]).await;
        let (avaia, point) = (at(1000.0, 1000.0), at(1400.0, 1000.0));
        assert_eq!(walk_route_available(&archive, avaia, point).await, Ok(true));
        let choices = pickup_choices(Some(&archive), true, point, Some(avaia)).await;
        assert!(choices.offer_xping);
        assert!(choices.suggest_avaia_walk);
        assert!(choices.avaia_distance_m.is_some_and(|d| d < 300));
    }

    #[tokio::test]
    async fn a_gap_in_the_network_is_no_walk() {
        // Two footways 100 m apart, never joined.
        let archive = kyiv(&[[(1000, 1000), (1150, 1000)], [(1400, 1000), (1550, 1000)]]).await;
        let (avaia, point) = (at(1000.0, 1000.0), at(1550.0, 1000.0));
        assert_eq!(
            walk_route_available(&archive, avaia, point).await,
            Ok(false)
        );
        let choices = pickup_choices(Some(&archive), true, point, Some(avaia)).await;
        assert!(choices.offer_xping);
        assert!(!choices.suggest_avaia_walk);
    }

    #[tokio::test]
    async fn a_nearer_stub_does_not_hide_the_connected_footway() {
        // An isolated 4 m stub under the Avaia, the footway 7 m away.
        let archive = kyiv(&[[(1000, 1000), (1400, 1000)], [(990, 1020), (1000, 1020)]]).await;
        let (avaia, point) = (at(995.0, 1019.0), at(1400.0, 1000.0));
        assert_eq!(walk_route_available(&archive, avaia, point).await, Ok(true));
    }

    #[tokio::test]
    async fn far_from_any_road_is_no_walk() {
        let archive = kyiv(&[[(1000, 1000), (1400, 1000)]]).await;
        let (avaia, point) = (at(1000.0, 1300.0), at(1400.0, 1000.0));
        assert_eq!(
            walk_route_available(&archive, avaia, point).await,
            Ok(false)
        );
    }

    #[tokio::test]
    async fn unpaid_far_or_unknown_never_reads_the_archive() {
        let bytes = archive(
            &[(TILE, roads_tile(&[[(1000, 1000), (1400, 1000)]]))],
            false,
        );
        let source = Counting(MemorySource(bytes), AtomicUsize::new(0));
        let archive = MapArchive::open(source).await.unwrap();
        let opened = archive_reads(&archive);
        let point = at(1400.0, 1000.0);
        for (paid, avaia) in [
            (false, Some(at(1000.0, 1000.0))),
            // More than 300 m away: about 1.5 km.
            (true, Some(at(0.0, 1000.0))),
            (true, None),
        ] {
            let choices = pickup_choices(Some(&archive), paid, point, avaia).await;
            assert!(!choices.suggest_avaia_walk);
            assert_eq!(choices.offer_xping, paid);
        }
        assert_eq!(archive_reads(&archive), opened);
        // No archive configured: Core still answers, without a walk.
        let none: Option<&MapArchive<MemorySource>> = None;
        let choices = pickup_choices(none, true, point, Some(at(1000.0, 1000.0))).await;
        assert!(choices.offer_xping && !choices.suggest_avaia_walk);
    }

    fn archive_reads(archive: &MapArchive<Counting>) -> usize {
        archive.source().1.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn a_broken_tile_fails_closed() {
        let archive = MapArchive::open(MemorySource(archive(&[(TILE, vec![0x1a, 0xff])], false)))
            .await
            .unwrap();
        let (avaia, point) = (at(1000.0, 1000.0), at(1400.0, 1000.0));
        assert_eq!(
            walk_route_available(&archive, avaia, point).await,
            Err(WalkError::Tile("malformed"))
        );
        let choices = pickup_choices(Some(&archive), true, point, Some(avaia)).await;
        assert!(choices.offer_xping && !choices.suggest_avaia_walk);
    }

    #[test]
    fn the_read_stays_bounded() {
        let kyiv = [30.5234, 50.4501];
        assert!(covering_tiles(kyiv, kyiv).unwrap().len() <= 4);
        assert_eq!(
            covering_tiles(kyiv, [30.9, 50.4501]),
            Err(WalkError::TooManyTiles)
        );
        assert_eq!(tile_of(30.5234, 50.4501), (9581, 5524));
        assert_eq!(tile_of(30.5234, 50.40), (9581, 5528));
    }

    #[test]
    fn the_archive_url_is_optional() {
        // SAFETY: tests touching this variable run only here.
        unsafe { std::env::remove_var("BOT_MAP_ARCHIVE_URL") };
        assert!(archive_source_from_env().unwrap().is_none());
    }
}

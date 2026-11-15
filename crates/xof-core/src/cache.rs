//! Caches: T1 (raw imagery tiles on disk, redb) and RAM LRUs.

use std::collections::HashMap;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use parking_lot::{MappedRwLockReadGuard, Mutex, RwLock, RwLockReadGuard};
use redb::{Database, Durability, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::provider::Fetched;
use crate::tile::TileCoord;

/// Zoom at which cache *areas* (the eviction unit) are defined. A z12 tile
/// is ~10 km, i.e. one ZL16 texture block.
pub const AREA_ZOOM: u8 = 12;

const TILES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tiles");
const AREAS: TableDefinition<&[u8], (u64, u64)> = TableDefinition::new("areas");

/// Negative ("no imagery") entries are re-checked after this many seconds.
const NEGATIVE_TTL: u64 = 30 * 24 * 3600;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The cache area a tile belongs to.
pub fn area_of(t: TileCoord) -> TileCoord {
    if t.z <= AREA_ZOOM {
        t
    } else {
        t.ancestor(t.z - AREA_ZOOM).unwrap()
    }
}

fn provider_byte(provider: &str) -> u8 {
    // Stable small hash; collisions between two configured providers would
    // only mix cache entries, never corrupt them (the key also stores z/x/y).
    provider.bytes().fold(0u8, |a, b| a.wrapping_mul(31).wrapping_add(b))
}

fn area_key(p: u8, a: TileCoord) -> [u8; 10] {
    let mut k = [0u8; 10];
    k[0] = p;
    k[1] = a.z;
    k[2..6].copy_from_slice(&a.x.to_be_bytes());
    k[6..10].copy_from_slice(&a.y.to_be_bytes());
    k
}

fn tile_key(p: u8, t: TileCoord) -> [u8; 19] {
    let mut k = [0u8; 19];
    k[..10].copy_from_slice(&area_key(p, area_of(t)));
    k[10] = t.z;
    k[11..15].copy_from_slice(&t.x.to_be_bytes());
    k[15..19].copy_from_slice(&t.y.to_be_bytes());
    k
}

/// One cached area, for the UI cache map.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AreaInfo {
    pub provider_byte: u8,
    pub area: TileCoord,
    pub last_access: u64,
    pub bytes: u64,
}

/// T1: raw imagery tiles keyed by provider and tile, grouped into areas for
/// LRU eviction.
pub struct TileStore {
    /// `None` while closed (cache being moved): every operation fails, which
    /// the engine treats as a cache miss.
    db: RwLock<Option<Database>>,
    path: Mutex<PathBuf>,
    /// Pending last-access updates, flushed in batches.
    touched: Mutex<HashMap<[u8; 10], u64>>,
}

fn create_db(path: &Path) -> anyhow::Result<Database> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let db = Database::create(path)?;
    let tx = db.begin_write()?;
    tx.open_table(TILES)?;
    tx.open_table(AREAS)?;
    tx.commit()?;
    Ok(db)
}

impl TileStore {
    pub fn open(path: &Path) -> anyhow::Result<Arc<Self>> {
        let db = create_db(path)?;
        Ok(Arc::new(Self {
            db: RwLock::new(Some(db)),
            path: Mutex::new(path.to_path_buf()),
            touched: Mutex::new(HashMap::new()),
        }))
    }

    /// The database file.
    pub fn path(&self) -> PathBuf {
        self.path.lock().clone()
    }

    /// Flush and close the database (to move or delete its file). Waits for
    /// in-flight operations.
    pub fn close(&self) -> anyhow::Result<()> {
        let _ = self.flush_access();
        self.db.write().take();
        Ok(())
    }

    /// Open (or create) the database at `path` and use it from now on.
    pub fn reopen(&self, path: &Path) -> anyhow::Result<()> {
        let mut g = self.db.write();
        g.take();
        *g = Some(create_db(path)?);
        *self.path.lock() = path.to_path_buf();
        Ok(())
    }

    fn db(&self) -> anyhow::Result<MappedRwLockReadGuard<'_, Database>> {
        RwLockReadGuard::try_map(self.db.read(), |d| d.as_ref())
            .map_err(|_| anyhow::anyhow!("imagery cache is closed (being moved)"))
    }

    pub fn get(&self, provider: &str, t: TileCoord) -> anyhow::Result<Option<Fetched>> {
        let p = provider_byte(provider);
        let tx = self.db()?.begin_read()?;
        let table = tx.open_table(TILES)?;
        let Some(v) = table.get(&tile_key(p, t)[..])? else {
            return Ok(None);
        };
        let v = v.value();
        if v.len() < 9 {
            return Ok(None);
        }
        let stored = u64::from_be_bytes(v[1..9].try_into().unwrap());
        self.touched.lock().insert(area_key(p, area_of(t)), now());
        Ok(match v[0] {
            0 => Some(Fetched::Data(Bytes::copy_from_slice(&v[9..]))),
            _ if now().saturating_sub(stored) < NEGATIVE_TTL => Some(Fetched::NoData),
            _ => None,
        })
    }

    pub fn put(&self, provider: &str, t: TileCoord, f: &Fetched) -> anyhow::Result<()> {
        let p = provider_byte(provider);
        let mut val = Vec::with_capacity(9 + if let Fetched::Data(b) = f { b.len() } else { 0 });
        let ts = now();
        match f {
            Fetched::Data(b) => {
                val.push(0);
                val.extend_from_slice(&ts.to_be_bytes());
                val.extend_from_slice(b);
            }
            Fetched::NoData => {
                val.push(1);
                val.extend_from_slice(&ts.to_be_bytes());
            }
        }
        let mut tx = self.db()?.begin_write()?;
        tx.set_durability(Durability::Eventual);
        {
            let mut tiles = tx.open_table(TILES)?;
            let key = tile_key(p, t);
            let old = tiles.insert(&key[..], &val[..])?.map(|o| o.value().len() as u64).unwrap_or(0);
            let mut areas = tx.open_table(AREAS)?;
            let ak = area_key(p, area_of(t));
            let (_, bytes) = areas.get(&ak[..])?.map(|v| v.value()).unwrap_or((0, 0));
            areas.insert(&ak[..], (ts, bytes.saturating_sub(old) + val.len() as u64))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Persist pending last-access timestamps.
    pub fn flush_access(&self) -> anyhow::Result<()> {
        let pending = std::mem::take(&mut *self.touched.lock());
        if pending.is_empty() {
            return Ok(());
        }
        let mut tx = self.db()?.begin_write()?;
        tx.set_durability(Durability::Eventual);
        {
            let mut areas = tx.open_table(AREAS)?;
            for (k, ts) in pending {
                let cur = areas.get(&k[..])?.map(|v| v.value());
                if let Some((last, bytes)) = cur {
                    areas.insert(&k[..], (last.max(ts), bytes))?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn total_bytes(&self) -> anyhow::Result<u64> {
        let tx = self.db()?.begin_read()?;
        let areas = tx.open_table(AREAS)?;
        let mut sum = 0;
        for e in areas.iter()? {
            sum += e?.1.value().1;
        }
        Ok(sum)
    }

    pub fn areas(&self) -> anyhow::Result<Vec<AreaInfo>> {
        let tx = self.db()?.begin_read()?;
        let areas = tx.open_table(AREAS)?;
        let mut out = Vec::new();
        for e in areas.iter()? {
            let (k, v) = e?;
            let k = k.value();
            let (last_access, bytes) = v.value();
            out.push(AreaInfo {
                provider_byte: k[0],
                area: TileCoord::new(
                    k[1],
                    u32::from_be_bytes(k[2..6].try_into().unwrap()),
                    u32::from_be_bytes(k[6..10].try_into().unwrap()),
                ),
                last_access,
                bytes,
            });
        }
        Ok(out)
    }

    /// Delete one area (all providers' tiles under that area key).
    pub fn evict_area(&self, provider_byte: u8, area: TileCoord) -> anyhow::Result<u64> {
        let ak = area_key(provider_byte, area);
        let mut tx = self.db()?.begin_write()?;
        tx.set_durability(Durability::Eventual);
        let freed;
        {
            let mut tiles = tx.open_table(TILES)?;
            let mut lo = ak.to_vec();
            lo.extend_from_slice(&[0u8; 9]);
            let mut hi = ak.to_vec();
            hi.extend_from_slice(&[0xffu8; 9]);
            let keys: Vec<Vec<u8>> = tiles
                .range(&lo[..]..=&hi[..])?
                .map(|e| e.map(|(k, _)| k.value().to_vec()))
                .collect::<Result<_, _>>()?;
            for k in keys {
                tiles.remove(&k[..])?;
            }
            let mut areas = tx.open_table(AREAS)?;
            freed = areas.remove(&ak[..])?.map(|v| v.value().1).unwrap_or(0);
        }
        tx.commit()?;
        Ok(freed)
    }

    /// Evict least-recently-used areas until the store is below `cap` bytes.
    pub fn evict_to(&self, cap: u64) -> anyhow::Result<u64> {
        self.flush_access()?;
        let mut areas = self.areas()?;
        let mut total: u64 = areas.iter().map(|a| a.bytes).sum();
        if total <= cap {
            return Ok(0);
        }
        areas.sort_by_key(|a| a.last_access);
        let mut freed = 0;
        for a in areas {
            if total <= cap {
                break;
            }
            let f = self.evict_area(a.provider_byte, a.area)?;
            total = total.saturating_sub(f);
            freed += f;
        }
        Ok(freed)
    }

    pub fn tile_count(&self) -> anyhow::Result<u64> {
        let tx = self.db()?.begin_read()?;
        Ok(tx.open_table(TILES)?.len()?)
    }
}

/// A byte-budgeted LRU cache.
pub struct ByteLru<K: Hash + Eq, V> {
    inner: Mutex<(lru::LruCache<K, (V, usize)>, usize)>,
    budget: usize,
}

impl<K: Hash + Eq, V: Clone> ByteLru<K, V> {
    pub fn new(budget: usize) -> Self {
        Self { inner: Mutex::new((lru::LruCache::unbounded(), 0)), budget }
    }

    pub fn get(&self, k: &K) -> Option<V> {
        self.inner.lock().0.get(k).map(|(v, _)| v.clone())
    }

    pub fn insert(&self, k: K, v: V, weight: usize) {
        let mut g = self.inner.lock();
        let (cache, used) = &mut *g;
        if let Some((_, w)) = cache.put(k, (v, weight)) {
            *used -= w;
        }
        *used += weight;
        while *used > self.budget {
            match cache.pop_lru() {
                Some((_, (_, w))) => *used -= w,
                None => break,
            }
        }
    }

    pub fn remove(&self, k: &K) {
        let mut g = self.inner.lock();
        let (cache, used) = &mut *g;
        if let Some((_, w)) = cache.pop(k) {
            *used -= w;
        }
    }

    pub fn used(&self) -> usize {
        self.inner.lock().1
    }

    pub fn len(&self) -> usize {
        self.inner.lock().0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Keys matching a predicate (for sweeps such as "upgrade degraded").
    pub fn keys_where(&self, mut f: impl FnMut(&K, &V) -> bool) -> Vec<K>
    where
        K: Clone,
    {
        self.inner.lock().0.iter().filter(|(k, (v, _))| f(k, v)).map(|(k, _)| k.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_roundtrip_and_evict() {
        let dir = tempfile::tempdir().unwrap();
        let s = TileStore::open(&dir.path().join("t.redb")).unwrap();
        let a = TileCoord::new(16, 19344, 24560);
        let b = TileCoord::new(16, 0, 0);
        assert!(s.get("esri", a).unwrap().is_none());
        s.put("esri", a, &Fetched::Data(Bytes::from(vec![7u8; 1000]))).unwrap();
        s.put("esri", b, &Fetched::NoData).unwrap();
        match s.get("esri", a).unwrap() {
            Some(Fetched::Data(d)) => assert_eq!(d.len(), 1000),
            other => panic!("{other:?}"),
        }
        assert!(matches!(s.get("esri", b).unwrap(), Some(Fetched::NoData)));
        assert!(s.get("bing", a).unwrap().is_none());
        assert_eq!(s.areas().unwrap().len(), 2);
        assert!(s.total_bytes().unwrap() >= 1000);
        // Evicting just below the total removes exactly one area.
        s.flush_access().unwrap();
        let freed = s.evict_to(1015).unwrap();
        assert!(freed > 0);
        assert_eq!(s.tile_count().unwrap(), 1);
    }

    #[test]
    fn byte_lru_budget() {
        let c = ByteLru::new(10);
        c.insert(1, "a", 4);
        c.insert(2, "b", 4);
        assert_eq!(c.get(&1), Some("a")); // 1 is now most recent
        c.insert(3, "c", 4); // evicts 2
        assert_eq!(c.get(&2), None);
        assert_eq!(c.used(), 8);
    }
}

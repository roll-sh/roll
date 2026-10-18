//! The module's global state: the index registry (`specDict_g`), the schema prefixes trie
//! (`SchemaPrefixes_g`), aliases, and the keyspace subscription's scratch state (`hashFields`,
//! `global_RenameFromKey`).

use super::index::Index;
use super::rsdict::RsDict;
use super::error::{Code, QErr};
use super::triemap::TrieMap;
use bytes::Bytes;

/// `maxIndexes_g`.
pub const MAX_INDEXES: usize = 200_000;

/// The keyspace event classes the module subscribes to (`Initialize_KeyspaceNotifications`):
/// generic, hash, string, expired, evicted and module (`trimmed`, `key_trimmed` and `loaded`
/// have no class of their own here).
pub const EVENT_CLASSES: u32 =
    crate::notify::GENERIC | crate::notify::HASH | crate::notify::STRING | crate::notify::EXPIRED | crate::notify::EVICTED | crate::notify::MODULE;

#[derive(Debug, Default)]
pub struct Global {
    /// `specDict_g`: indexes by name.
    pub indexes: RsDict<Index>,
    /// `SchemaPrefixes_g`: a prefix → the indexes it routes keys to, in creation order.
    pub prefixes: TrieMap<Vec<Bytes>>,
    /// `AliasTable_g`: alias → index name.
    pub aliases: RsDict<Bytes>,
    pub next_spec_id: u64,
    /// The module subscribed to keyspace events (its first index was created).
    pub subscribed: bool,
    /// `hashFields`: the fields the last H* command named (the command filter).
    pub hash_fields: Option<Vec<Bytes>>,
    /// `global_RenameFromKey`.
    pub rename_from: Option<Bytes>,
    /// `spellCheckDicts`: spelling dictionaries (FT.DICTADD), their terms as runes in trie order.
    pub dicts: RsDict<std::collections::BTreeSet<Vec<u16>>>,
    /// `maxIndexes_g` when FT.DEBUG SET_MAX_INDEXES changed it.
    pub max_indexes: Option<usize>,
    /// `g_CursorsList`: FT.AGGREGATE WITHCURSOR's cursors (they outlive their index).
    pub cursors: super::cursor::Cursors,
}

impl Global {
    pub fn get(&self, name: &[u8]) -> Option<&Index> {
        self.indexes.get(name)
    }

    pub fn get_mut(&mut self, name: &[u8]) -> Option<&mut Index> {
        self.indexes.get_mut(name)
    }

    /// `IndexSpec_LoadUnsafe`: an index by name, or by alias.
    pub fn resolve(&self, name: &[u8]) -> Option<&Bytes> {
        if let Some((k, _)) = self.indexes.get_key_value(name) {
            return Some(k);
        }
        self.aliases.get(name)
    }

    /// `maxIndexes_g`.
    pub fn max_indexes(&self) -> usize {
        self.max_indexes.unwrap_or(MAX_INDEXES)
    }

    /// `Indexes_LoadIndexSpecUnsafeEx`: by name (or alias, unless `no_alias`), counting a use.
    pub fn load(&mut self, name: &[u8], no_alias: bool) -> Option<Bytes> {
        let n = if no_alias { self.indexes.get_key_value(name).map(|(k, _)| k) } else { self.resolve(name) }.cloned()?;
        if let Some(ix) = self.indexes.get_mut(&n) {
            ix.counter += 1;
        }
        Some(n)
    }

    /// `IndexSpec_OnAcquire`: a lookup by name or alias counts a use (FT.INFO's `number_of_uses`).
    pub fn count_use(&mut self, name: &[u8]) {
        if let Some(n) = self.resolve(name).cloned()
            && let Some(ix) = self.indexes.get_mut(&n)
        {
            ix.counter += 1;
        }
    }

    /// `onFlush` (FLUSHDB/FLUSHALL): every index, alias, dictionary and cursor goes.
    pub fn flush(&mut self) {
        // `Indexes_Free`: each spec is deleted from the registry, whose tables never shrink.
        for name in self.indexes.keys().cloned().collect::<Vec<_>>() {
            self.remove(&name);
        }
        self.prefixes = TrieMap::new();
        self.cursors.clear();
        self.aliases.clear();
        self.dicts.clear();
    }

    /// `IndexAlias_Add`.
    pub fn add_alias(&mut self, alias: Bytes, target: &Bytes) -> Result<(), QErr> {
        if !self.aliases.add(alias.clone(), target.clone()) {
            return Err(QErr::new(Code::IndexExists, "Alias already exists"));
        }
        if let Some(ix) = self.indexes.get_mut(target) {
            ix.aliases.push(alias);
        }
        Ok(())
    }

    /// `IndexAlias_Del`: the alias must be one of the index's (compared without case).
    pub fn del_alias(&mut self, alias: &[u8], target: &Bytes) -> Result<(), QErr> {
        let Some(ix) = self.indexes.get_mut(target) else { return Ok(()) };
        let Some(i) = ix.aliases.iter().position(|a| a.eq_ignore_ascii_case(alias)) else {
            return Err(QErr::new(Code::NoIndex, "Alias does not belong to provided spec"));
        };
        ix.aliases.swap_remove(i);
        self.aliases.delete(alias);
        Ok(())
    }

    /// Registers a new index and its prefixes (`SchemaPrefixes_Add`).
    pub fn add(&mut self, index: Index) {
        let name = index.spec.name.clone();
        for p in &index.spec.rule.prefixes {
            match self.prefixes.get_mut(p) {
                Some(v) => v.push(name.clone()),
                None => {
                    self.prefixes.insert(p, vec![name.clone()]);
                }
            }
        }
        self.indexes.add(name, index);
    }

    /// `Indexes_RemoveSpecFromGlobals`: the index, its prefixes and aliases go.
    pub fn remove(&mut self, name: &[u8]) -> Option<Index> {
        let index = self.indexes.delete(name)?;
        for p in &index.spec.rule.prefixes {
            if let Some(v) = self.prefixes.get_mut(p)
                && let Some(i) = v.iter().position(|n| n.as_ref() == name)
            {
                v.swap_remove(i);
                if v.is_empty() {
                    self.prefixes.remove(p);
                }
            }
        }
        // `IndexSpec_ClearAliases`.
        for a in &index.aliases {
            self.aliases.delete(a);
        }
        // An alias the index's list lost track of (FT.ALIASDEL with another case removes the list
        // entry but not the table's) would dangle in the module, a use-after-free; here it goes.
        let stale: Vec<Bytes> = self.aliases.iter().filter(|(_, t)| t.as_ref() == name).map(|(a, _)| a.clone()).collect();
        for a in stale {
            self.aliases.delete(&a);
        }
        Some(index)
    }

    /// The indexes whose prefixes match a key, each once, in prefix order (`TrieMap_FindPrefixes`).
    pub fn matching(&self, key: &[u8]) -> Vec<Bytes> {
        let mut out: Vec<Bytes> = Vec::new();
        for names in self.prefixes.find_prefixes(key) {
            for n in names {
                if !out.contains(n) {
                    out.push(n.clone());
                }
            }
        }
        out
    }
}

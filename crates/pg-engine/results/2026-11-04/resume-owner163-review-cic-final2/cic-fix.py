from pathlib import Path
r=Path('/home/meszmate/roll/.claude/worktrees/agent-a3512214927146502/crates/pg-engine/src')
p=r/'exec/rowsize.rs';s=p.read_text();s += '''

/// Size validation shared by serial construction and concurrent-build preflight. Tuple
/// formation runs for every heap row; page limits are reported after uniqueness.
#[derive(Default)]
pub struct BuildSizes {
    btree: Option<(Vec<u8>, usize, Vec<u8>)>,
    gin: Option<GinFormError>,
}

impl BuildSizes {
    pub fn row(&mut self, t: &Table, bix: &BoundIndex, vals: &[Value], row: &[Value], dkey: &[u8], ctx: &ExecCtx) -> Result<()> {
        if bix.index.method == "gin" {
            if let Some(e) = gin_form_error(bix, vals, ctx)
                && self.gin.as_ref().is_none_or(|first| e.precedes(first))
            {
                self.gin = Some(e);
            }
        } else {
            on_form(t, bix, Some(vals), row, ctx)?;
        }
        if bix.index.method == "btree"
            && let Some(n) = btree_oversized(t, bix, vals, row, ctx)?
        {
            let k = crate::exec::scan::index_entry_key(t, &bix.index, vals, dkey, &bix.colls);
            if self.btree.as_ref().is_none_or(|first| k < first.0) {
                self.btree = Some((k, n, dkey.to_vec()));
            }
        }
        Ok(())
    }

    pub fn finish(self, t: &Table, bix: &BoundIndex, ctx: &ExecCtx) -> Result<()> {
        if let Some(e) = self.gin {
            return Err(e.error);
        }
        if let Some((_, n, dkey)) = self.btree {
            let tid = crate::exec::dml::row_tid(t, &dkey, ctx);
            return Err(btree_error(t, bix, n, tid, ctx));
        }
        Ok(())
    }
}
''';s=s.replace('pub struct GinFormError','struct GinFormError').replace('pub fn gin_form_error','fn gin_form_error');p.write_text(s)
p=r/'session/ddl/table.rs';s=p.read_text();a=s.index('    /// The error building index');b=s.index('    /// Remove an index',a);part=s[a:b]
part=part.replace("    /// The error building index `ix` would stop at (computing a key, or a unique index's\n    /// duplicate; None: none), found without building it.","    /// The error building index `ix` would stop at, found without writing entries.\n    /// Concurrent builds commit their INVALID catalog row when this preflight fails.")
part=part.replace('index_duplicate(', 'index_build_error(').replace('let Some(bix)', 'let Some(mut bix)')
part=part.replace('        let blocks =', '        // A concurrent REINDEX copy has its own name before it is in the catalog.\n        bix.index = Arc::new(ix.clone());\n        let mut sizes = crate::exec::rowsize::BuildSizes::default();\n        let mut duplicate = None;\n        let blocks =',1)
part=part.replace('            if ix.unique &&', '            if let Err(e) = sizes.row(t, &bix, &vals, &row, &dkey, self.ctx) {\n                return Ok(Some(e));\n            }\n            if duplicate.is_none() && ix.unique &&')
part=part.replace('return Ok(Some(self.duplicate_key(t, &bix, first.as_deref().unwrap_or(&vals), &ix.name)));','duplicate = Some(self.duplicate_key(t, &bix, first.as_deref().unwrap_or(&vals), &ix.name));')
part=part.replace('        Ok(None)\n    }','        if duplicate.is_some() {\n            return Ok(duplicate);\n        }\n        Ok(sizes.finish(t, &bix, self.ctx).err())\n    }')
s=s[:a]+part+s[b:]
a=s.index('    pub(super) fn build_index(');b=s.index('    pub(super) fn duplicate_key',a);part=s[a:b]
a1=part.index('        // (A btree entry past');b1=part.index('        let mut step',a1)
part=part[:a1]+'        let mut sizes = crate::exec::rowsize::BuildSizes::default();\n        let mut duplicate = None;\n'+part[b1:]
a1=part.index('            if ix.method == "gin"');b1=part.index('            if quad',a1)
part=part[:a1]+'            sizes.row(t, &bix, vals, row, dkey, counts)?;\n'+part[b1:]
a1=part.index('        if let Some(e) = gin_error');b1=part.index('        if serial {\n            progress.loaded',a1)
part=part[:a1]+'''        if let Some(e) = duplicate {
            return Err(e);
        }
        sizes.finish(t, &bix, self.ctx)?;
'''+part[b1:]
s=s[:a]+part+s[b:];s=s.replace('self.index_duplicate(', 'self.index_build_error(')
s=s.replace('let copy_name = format!("{}_ccnew", ix.name);','let copy_name = self.cat.choose_name(t.schema, &Catalog::object_name(&ix.name, &[], "ccnew"));');p.write_text(s)
p=r/'session/ddl/objects.rs';s=p.read_text().replace('self.index_duplicate(', 'self.index_build_error(');p.write_text(s)

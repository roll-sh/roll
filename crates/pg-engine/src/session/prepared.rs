//! PREPARE TRANSACTION, COMMIT PREPARED and ROLLBACK PREPARED (PrepareTransaction and
//! FinishPreparedTransaction), over the engine's registry of prepared transactions
//! (`txn::twophase`).

use super::{Session, TxnStatus};
use crate::error::{Error, Result, code};
use crate::txn::twophase::{self, Info};

impl Session {
    /// PrepareTransaction for the open block: the transaction is checked as a commit
    /// would check it, its state record stored, and it leaves the session as prepared
    /// transaction `gid`. On an error it rolls back, as Postgres's does.
    pub(super) fn prepare_txn(&mut self, gid: &str) -> Result<()> {
        // PreCommit_Portals: a holdable cursor of this transaction can't be kept.
        if self.state.cursors.borrow().values().any(|c| c.hold && c.lazy.is_some()) {
            let _ = self.finish_txn(false);
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot PREPARE a transaction that has created a cursor WITH HOLD",
            ));
        }
        // AfterTriggerFireDeferred.
        if self.txn.as_ref().is_some_and(|t| {
            !t.borrow().deferred.is_empty() || !t.borrow().deferred_triggers.is_empty()
        }) && let Err(e) = self.with_ctx(&[], |ctx| {
            crate::exec::trigger::fire_deferred(ctx, None)?;
            crate::exec::dml::check_deferred(ctx, None)
        }) {
            let _ = self.finish_txn(false);
            return Err(e);
        }
        // XACT_EVENT_PRE_PREPARE: postgres_fdw refuses.
        if let Err(e) = crate::ext::postgres_fdw::connection::pre_prepare(&self.state) {
            let _ = self.finish_txn(false);
            return Err(e);
        }
        self.ensure_txn();
        let tp = self.state.db.twophase();
        let mut reserved = false;
        let r = self.prepare_steps(gid, &mut reserved);
        let info = match r {
            Ok(info) => info,
            Err(e) => {
                if reserved {
                    tp.remove(gid);
                }
                let _ = self.finish_txn(false);
                return Err(e);
            }
        };
        let mut t = self.txn.take().unwrap().into_inner();
        t.detach_prepared();
        tp.prepared(&info.gid, t);
        // The rest of a commit's cleanup, as far as the session is concerned (PREPARE acts
        // as COMMIT for settings; cursors of the transaction close).
        self.implicit = false;
        self.status = TxnStatus::Idle;
        self.txn_opts = None;
        self.state.cursors.borrow_mut().retain(|_, c| c.hold && c.lazy.is_none());
        self.state.listen_actions.borrow_mut().clear();
        self.state.settings.borrow_mut().end_transaction(true);
        self.state.db.touch();
        Ok(())
    }

    /// PrepareTransaction from PreCommit_CheckForSerializationFailure to EndPrepare: the
    /// checks in Postgres's order, the gid reserved (`reserved` set once it is), the state
    /// record stored.
    fn prepare_steps(&mut self, gid: &str, reserved: &mut bool) -> Result<Info> {
        let txn = self.txn.as_ref().unwrap();
        txn.borrow_mut().prepare_checks()?;
        if self.state.temp_used.get() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot PREPARE a transaction that has operated on temporary objects",
            ));
        }
        if txn.borrow().has_exported_snapshots() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot PREPARE a transaction that has exported snapshots",
            ));
        }
        let cat = self.state.db.catalog();
        let info = Info {
            xid: txn.borrow_mut().xid(),
            gid: gid.to_string(),
            prepared_at: crate::types::datetime::Clock::system().now,
            owner: self.state.current_role(&cat),
            db: self.state.db.oid,
        };
        let id = txn.borrow().id;
        let tp = self.state.db.twophase();
        tp.reserve(info.clone())?;
        *reserved = true;
        // AtPrepare_Notify.
        if !txn.borrow().notifies.is_empty() || !self.state.listen_actions.borrow().is_empty() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot PREPARE a transaction that has executed LISTEN, UNLISTEN, or NOTIFY",
            ));
        }
        // AtPrepare_Locks.
        if self.state.db.locks.held_by_both(id, self.state.lock_owner) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot PREPARE while holding both session-level and transaction-level locks on the same object",
            ));
        }
        // EndPrepare: the state record, durable before PREPARE returns. (With the change
        // stream's records and its PREPARE record, when the transaction logs its changes.)
        let rec = txn.borrow().prepared_record(&info)?;
        let writes = vec![(twophase::record_key(gid), Some(rec.into()))];
        crate::repl::log::prepare(&self.state.db, &mut txn.borrow_mut(), &info, writes)?;
        Ok(info)
    }

    /// FinishPreparedTransaction: commit or roll back prepared transaction `gid`.
    pub(super) fn finish_prepared(&mut self, gid: &str, commit: bool) -> Result<()> {
        let tp = self.state.db.twophase();
        let cat = self.state.db.catalog();
        let user = self.state.current_role(&cat);
        let (_, mut t) = tp.lock(gid, user, cat.is_superuser(user), self.state.db.oid)?;
        let stats = std::mem::take(&mut t.pgstat);
        let r = if commit {
            t.remove_state_record(gid);
            self.state.cluster.commit(t).map(|_| ())
        } else {
            for (name, owner) in std::mem::take(&mut t.db_owner_undo).into_iter().rev() {
                let _ = self.state.cluster.set_database_owner(&name, &owner);
            }
            for (name, o) in std::mem::take(&mut t.db_options_undo).into_iter().rev() {
                let _ = self.state.cluster.set_database_options(&name, o);
            }
            let r = crate::repl::log::rollback_prepared(&self.state.db, &t, vec![(twophase::record_key(gid), None)]);
            t.rollback();
            r.map(|_| ())
        };
        tp.remove(gid);
        crate::exec::pgstat::end_prepared(&self.state, stats, commit && r.is_ok(), &self.state.db.catalog());
        r
    }
}

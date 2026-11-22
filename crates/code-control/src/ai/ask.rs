//! One question to a repo owner's model for another product (roll apps' "Why did it fail?"):
//! the same provider choice, cap and metering as a reviewer's reply, with the asker's prompt and
//! answer schema. The answer is the model's: untrusted data for the asker to check.

use super::store::UsageRow;
use super::month_of;
use crate::{Code, Error, Result, new_id};
use code_review::pricing::{self, Kind, KeySource, Month, PriceInput};
use code_review::provider::{Effort, Request, Stop, SystemBlock};
use serde_json::Value;

/// What to ask: a fixed system prompt (cached), the case, and the answer's JSON schema.
#[derive(Clone, Debug)]
pub struct ModelAsk {
    /// Which product asks, metered as the usage row's `kind` (`explain`).
    pub kind: &'static str,
    pub system: String,
    pub user: String,
    pub schema: Value,
    pub schema_name: &'static str,
}

/// The model's answer, when it gave one, and which model it was.
#[derive(Clone, Debug)]
pub struct ModelAnswer {
    /// `None`: it declined or was cut off.
    pub json: Option<Value>,
    pub model: String,
}

impl Code {
    /// Ask `repo`'s (`owner/name`) owner's model: their own provider when they set one, else the
    /// operator's hosted one within the owner's monthly cap, priced and metered like a reply (as
    /// `ask.kind`, counted with chats).
    pub async fn ask_model(&self, repo: &str, ask: &ModelAsk) -> Result<ModelAnswer> {
        let errs = code_review::schema::check(&ask.schema);
        if !errs.is_empty() {
            return Err(Error::Internal(anyhow::anyhow!("an answer schema structured outputs reject: {}", errs.join("; "))));
        }
        let (owner, name) = repo.split_once('/').ok_or_else(|| Error::Invalid("a repo is owner/name".into()))?;
        let r = self.find_repo(owner, name)?;
        let no_model = || Error::Unprocessable(format!("{owner} has no model provider: set one (PUT /v1/code/owners/{owner}/review-provider), or the server's (ROLL_REVIEW_PROVIDER)"));
        let mut chosen = self.choose_provider(&r.owner_id, false).await?.ok_or_else(no_model)?;
        let public = Self::is_public(&r);
        let month = month_of(crate::db::now());
        let input = |key| PriceInput { kind: Kind::Chat, mode: code_review::Mode::Standard, key, files: 0, public_repo: public, human_author: true };
        let mut reservation = None;
        if chosen.key == KeySource::Hosted {
            let billing = self.db.billing(&r.owner_id)?;
            let estimate = |_| pricing::price(&self.config.review.rates, &input(KeySource::Hosted), &Month::default());
            match self.reserve(&r.owner_id, &month, billing.monthly_cap_usd, estimate)? {
                Some(g) => reservation = Some(g),
                None => match (billing.on_cap.as_str(), self.choose_provider(&r.owner_id, true).await?) {
                    ("byok_fallback", Some(b)) => chosen = b,
                    _ => return Err(Error::Unprocessable(format!("{owner}'s monthly AI cap (${:.2}) is reached", billing.monthly_cap_usd.unwrap_or_default()))),
                },
            }
        }
        let req = Request {
            model: String::new(),
            system: vec![SystemBlock { text: ask.system.clone(), cache: true }],
            user: ask.user.clone(),
            schema: ask.schema.clone(),
            schema_name: ask.schema_name,
            effort: Effort::Medium,
            max_tokens: 4000,
        };
        let res = chosen.provider.complete(&req).await.map_err(|e| match e {
            code_review::ProviderError::Rejected { .. } | code_review::ProviderError::Config(_) => Error::Unprocessable(e.to_string()),
            e => Error::Unavailable(e.to_string()),
        })?;
        let model = if res.model.is_empty() { chosen.provider.model().to_string() } else { res.model.clone() };
        let price = pricing::price(&self.config.review.rates, &input(chosen.key), &Month::default());
        let metered = UsageRow {
            id: new_id("use"),
            owner_id: r.owner_id.clone(),
            repo_id: r.id.clone(),
            change_id: None,
            review_id: None,
            kind: ask.kind.into(),
            mode: "standard".into(),
            chosen_by: ask.kind.into(),
            reasons: Vec::new(),
            speed: "balanced".into(),
            provider: chosen.name.into(),
            model: model.clone(),
            key_source: chosen.key.as_str().into(),
            files_reviewed: 0,
            input_tokens: res.usage.input_tokens,
            output_tokens: res.usage.output_tokens,
            cache_write_tokens: res.usage.cache_write_tokens,
            cache_read_tokens: res.usage.cache_read_tokens,
            provider_cost_usd: pricing::provider_cost(&model, &res.usage, &self.config.review.model_rates),
            list_usd: price.list_usd,
            price_usd: price.charged_usd,
            free: false,
            capped: false,
            human_author: true,
            author: String::new(),
            public,
            month,
            created_at: crate::db::now(),
        };
        self.meter(&metered, reservation.as_mut())?;
        // Only a finished answer is one (`stop_reason` before the content).
        let json = match res.stop {
            Stop::EndTurn => res.json,
            _ => None,
        };
        Ok(ModelAnswer { json, model })
    }
}

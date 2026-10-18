//! The backlog tool's gRPC service, over the tool's own store. Every write is
//! guarded first, then checked against the limits, then stored.

use std::collections::HashSet;
use std::sync::Arc;

use harold_api::tools::backlog::v1 as pb;
use harold_api::tools::backlog::v1::backlog_service_server::BacklogService;
use tonic::{Request, Response, Status};
use tracing::info;

use super::guard::{Guard, Refused};
use super::markdown::{self, Counts, ParseError};
use super::schema::{self, Item, Note, NoteKind, Place, QueueItem};
use super::store::{BacklogStore, Refusal, StoreError};
use super::{NAME, now_ms};

pub(super) struct BacklogToolService<S> {
    store: Arc<S>,
    guard: Guard,
}

impl<S: BacklogStore> BacklogToolService<S> {
    pub(super) fn new(store: Arc<S>, guard: Guard) -> Self {
        Self { store, guard }
    }

    /// Guards a write to `product`'s backlog; a refusal is logged.
    fn may_write_product(&self, caller_pane: &str, product: &str) -> Result<(), Status> {
        self.guard
            .may_write_product(caller_pane, product)
            .map_err(|refused| {
                info!(
                    tool = NAME,
                    product,
                    reason = refused.reason(),
                    "backlog write refused"
                );
                denied(refused)
            })
    }

    /// Guards a write to the queue; a refusal is logged.
    fn may_write_queue(&self, caller_pane: &str) -> Result<(), Status> {
        self.guard.may_write_queue(caller_pane).map_err(|refused| {
            info!(
                tool = NAME,
                reason = refused.reason(),
                "backlog write refused"
            );
            denied(refused)
        })
    }

    /// Logs a change to the queue with the parked count after it.
    async fn queue_changed(&self, parked: Option<u32>) -> Result<(), Status> {
        let count = match parked {
            Some(count) => count,
            None => {
                // No done item is as late as `i64::MAX`, so only Now and the parked are read.
                let listing = self.store.list_queue(i64::MAX).await.map_err(unavailable)?;
                count(listing.parked.len())
            }
        };
        info!(tool = NAME, count, "queue changed");
        Ok(())
    }
}

fn invalid(field: &str) -> Status {
    Status::invalid_argument(format!("invalid {field}"))
}

fn denied(refused: Refused) -> Status {
    let reason = refused.reason();
    match refused {
        Refused::NoPane | Refused::WrongProduct | Refused::NotChief => {
            Status::permission_denied(reason)
        }
        Refused::UnknownProduct => Status::not_found(reason),
        Refused::RegistryUnreadable => Status::unavailable(reason),
    }
}

fn refused(refusal: Refusal) -> Status {
    let reason = refusal.reason();
    match refusal {
        Refusal::NotFound => Status::not_found(reason),
        Refusal::Full
        | Refusal::AppendOnly
        | Refusal::NotEmpty
        | Refusal::NotParked
        | Refusal::AlreadyDone => Status::failed_precondition(reason),
    }
}

fn unavailable(_: StoreError) -> Status {
    Status::unavailable("the backlog database is unavailable")
}

fn unparsed(error: ParseError) -> Status {
    match error {
        ParseError::Full => Status::failed_precondition(error.to_string()),
        ParseError::Unparsed(_) | ParseError::Invalid(_) => {
            Status::invalid_argument(error.to_string())
        }
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// An id list for a ranking or an order: at least one id, none repeated.
fn valid_ids(ids: &[String]) -> Result<(), Status> {
    let distinct: HashSet<&String> = ids.iter().collect();
    match !ids.is_empty() && distinct.len() == ids.len() {
        true => Ok(()),
        false => Err(invalid("ids")),
    }
}

fn kind_from_proto(kind: &str) -> Result<NoteKind, Status> {
    NoteKind::parse(kind).ok_or_else(|| invalid("kind"))
}

fn item_from_proto(item: pb::Item) -> Item {
    Item {
        id: item.id,
        title: item.title,
        value: item.value,
        size: item.size,
        risk: item.risk,
        reason: item.reason,
        paths: item.paths,
        after: item.after,
        evidence: item.evidence,
        rank: 0,
        updated_at_ms: 0,
    }
}

fn item_to_proto(item: Item) -> pb::Item {
    pb::Item {
        id: item.id,
        title: item.title,
        value: item.value,
        size: item.size,
        risk: item.risk,
        reason: item.reason,
        paths: item.paths,
        after: item.after,
        evidence: item.evidence,
        rank: item.rank,
        updated_at_ms: item.updated_at_ms,
    }
}

fn note_to_proto(note: Note) -> pb::Note {
    pb::Note {
        id: note.id,
        kind: note.kind.as_str().into(),
        title: note.title,
        body: note.body,
        created_at_ms: note.created_at_ms,
        updated_at_ms: note.updated_at_ms,
    }
}

fn queue_item_to_proto(item: QueueItem) -> pb::QueueItem {
    pb::QueueItem {
        id: item.id,
        state: item.state.as_str().into(),
        position: item.position,
        text: item.text,
        done_at_ms: item.done_at_ms,
        updated_at_ms: item.updated_at_ms,
    }
}

fn counts_to_proto(counts: Counts) -> pb::ImportCounts {
    pb::ImportCounts {
        items: counts.items,
        rule: counts.rule,
        history: counts.history,
        held: counts.held,
        left_out: counts.left_out,
        now: counts.now,
        parked: counts.parked,
        done: counts.done,
    }
}

/// An import or export names a product, or asks for the queue with no product.
fn check_target(queue: bool, product: &str) -> Result<(), Status> {
    match queue == product.is_empty() {
        true => Ok(()),
        false => Err(invalid("product")),
    }
}

/// When the imported done items were done: the queue file's date at local midnight
/// for the caller's UTC offset, or now when the file has no date.
fn done_at_ms(date: Option<time::Date>, utc_offset_seconds: i32) -> Result<i64, Status> {
    let Some(date) = date else {
        return Ok(now_ms());
    };
    let offset = time::UtcOffset::from_whole_seconds(utc_offset_seconds)
        .map_err(|_| invalid("utc_offset_seconds"))?;
    let midnight = date.midnight().assume_offset(offset);
    Ok((midnight.unix_timestamp_nanos() / 1_000_000) as i64)
}

#[tonic::async_trait]
impl<S: BacklogStore> BacklogService for BacklogToolService<S> {
    async fn list_products(
        &self,
        _request: Request<pb::ListProductsRequest>,
    ) -> Result<Response<pb::ListProductsResponse>, Status> {
        let products = self.guard.products().map_err(denied)?;
        let counts = self.store.item_counts().await.map_err(unavailable)?;
        let products = products
            .into_iter()
            .map(|(name, repo)| pb::Product {
                item_count: counts.get(&name).copied().unwrap_or(0),
                repo: repo.display().to_string(),
                name,
            })
            .collect();
        Ok(Response::new(pb::ListProductsResponse { products }))
    }

    async fn put_item(
        &self,
        request: Request<pb::PutItemRequest>,
    ) -> Result<Response<pb::PutItemResponse>, Status> {
        let pb::PutItemRequest {
            product,
            item,
            caller_pane,
        } = request.into_inner();
        self.may_write_product(&caller_pane, &product)?;
        let item = item_from_proto(item.ok_or_else(|| invalid("item"))?);
        schema::validate_item(&item).map_err(|field| invalid(&field))?;
        let id = item.id.clone();
        let rank = self
            .store
            .put_item(&product, item, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        info!(tool = NAME, product, item_id = %id, "backlog item stored");
        Ok(Response::new(pb::PutItemResponse { id, rank }))
    }

    async fn rank_items(
        &self,
        request: Request<pb::RankItemsRequest>,
    ) -> Result<Response<pb::RankItemsResponse>, Status> {
        let pb::RankItemsRequest {
            product,
            ids,
            caller_pane,
        } = request.into_inner();
        self.may_write_product(&caller_pane, &product)?;
        valid_ids(&ids)?;
        let count = self
            .store
            .rank_items(&product, &ids, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        info!(tool = NAME, product, count, "backlog ranked");
        Ok(Response::new(pb::RankItemsResponse { count }))
    }

    async fn remove_item(
        &self,
        request: Request<pb::RemoveItemRequest>,
    ) -> Result<Response<pb::RemoveItemResponse>, Status> {
        let pb::RemoveItemRequest {
            product,
            id,
            reason,
            caller_pane,
        } = request.into_inner();
        self.may_write_product(&caller_pane, &product)?;
        if !schema::under(&reason, schema::FIELD_UNDER) {
            return Err(invalid("reason"));
        }
        // An empty reason is no reason, so no `history` note is written.
        let reason = (!reason.is_empty()).then_some(reason.as_str());
        self.store
            .remove_item(&product, &id, reason, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        info!(tool = NAME, product, item_id = %id, "backlog item removed");
        Ok(Response::new(pb::RemoveItemResponse {}))
    }

    async fn list_items(
        &self,
        request: Request<pb::ListItemsRequest>,
    ) -> Result<Response<pb::ListItemsResponse>, Status> {
        let product = request.into_inner().product;
        let items = self
            .store
            .list_items(&product)
            .await
            .map_err(unavailable)?
            .into_iter()
            .map(item_to_proto)
            .collect();
        Ok(Response::new(pb::ListItemsResponse { items }))
    }

    async fn add_note(
        &self,
        request: Request<pb::AddNoteRequest>,
    ) -> Result<Response<pb::AddNoteResponse>, Status> {
        let pb::AddNoteRequest {
            product,
            kind,
            title,
            body,
            caller_pane,
        } = request.into_inner();
        self.may_write_product(&caller_pane, &product)?;
        let kind = kind_from_proto(&kind)?;
        schema::validate_note(&title, &body).map_err(|field| invalid(&field))?;
        let id = self
            .store
            .add_note(&product, kind, &title, &body, now_ms())
            .await
            .map_err(unavailable)?;
        info!(tool = NAME, product, note_id = id, "backlog note added");
        Ok(Response::new(pb::AddNoteResponse { id }))
    }

    async fn update_note(
        &self,
        request: Request<pb::UpdateNoteRequest>,
    ) -> Result<Response<pb::UpdateNoteResponse>, Status> {
        let pb::UpdateNoteRequest {
            product,
            id,
            title,
            body,
            caller_pane,
        } = request.into_inner();
        self.may_write_product(&caller_pane, &product)?;
        schema::validate_note(&title, &body).map_err(|field| invalid(&field))?;
        self.store
            .update_note(&product, id, &title, &body, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        Ok(Response::new(pb::UpdateNoteResponse {}))
    }

    async fn remove_note(
        &self,
        request: Request<pb::RemoveNoteRequest>,
    ) -> Result<Response<pb::RemoveNoteResponse>, Status> {
        let pb::RemoveNoteRequest {
            product,
            id,
            caller_pane,
        } = request.into_inner();
        self.may_write_product(&caller_pane, &product)?;
        self.store
            .remove_note(&product, id)
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        Ok(Response::new(pb::RemoveNoteResponse {}))
    }

    async fn list_notes(
        &self,
        request: Request<pb::ListNotesRequest>,
    ) -> Result<Response<pb::ListNotesResponse>, Status> {
        let pb::ListNotesRequest {
            product,
            kind,
            last,
        } = request.into_inner();
        let kind = match kind.is_empty() {
            true => None,
            false => Some(kind_from_proto(&kind)?),
        };
        let last = (last > 0).then_some(last);
        let notes = self
            .store
            .list_notes(&product, kind, last)
            .await
            .map_err(unavailable)?
            .into_iter()
            .map(note_to_proto)
            .collect();
        Ok(Response::new(pb::ListNotesResponse { notes }))
    }

    async fn add_queue_item(
        &self,
        request: Request<pb::AddQueueItemRequest>,
    ) -> Result<Response<pb::AddQueueItemResponse>, Status> {
        let pb::AddQueueItemRequest {
            text,
            now,
            at,
            caller_pane,
        } = request.into_inner();
        self.may_write_queue(&caller_pane)?;
        schema::validate_queue_text(&text).map_err(|field| invalid(&field))?;
        let place = match (now, at) {
            (false, 0) => Place::End,
            (true, 0) => Place::Now,
            (false, at) => Place::At(at),
            // Now is not a parked position.
            (true, _) => return Err(invalid("at")),
        };
        let item = self
            .store
            .add_queue_item(&text, place, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(|field| invalid(&field))?;
        self.queue_changed(None).await?;
        Ok(Response::new(pb::AddQueueItemResponse {
            item: Some(queue_item_to_proto(item)),
        }))
    }

    async fn set_now(
        &self,
        request: Request<pb::SetNowRequest>,
    ) -> Result<Response<pb::SetNowResponse>, Status> {
        let pb::SetNowRequest { id, caller_pane } = request.into_inner();
        self.may_write_queue(&caller_pane)?;
        let (now, parked) = self
            .store
            .set_now(&id, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        self.queue_changed(Some(parked)).await?;
        Ok(Response::new(pb::SetNowResponse { now, parked }))
    }

    async fn order_queue(
        &self,
        request: Request<pb::OrderQueueRequest>,
    ) -> Result<Response<pb::OrderQueueResponse>, Status> {
        let pb::OrderQueueRequest { ids, caller_pane } = request.into_inner();
        self.may_write_queue(&caller_pane)?;
        valid_ids(&ids)?;
        let count = self
            .store
            .order_queue(&ids, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        self.queue_changed(Some(count)).await?;
        Ok(Response::new(pb::OrderQueueResponse { count }))
    }

    async fn update_queue_item(
        &self,
        request: Request<pb::UpdateQueueItemRequest>,
    ) -> Result<Response<pb::UpdateQueueItemResponse>, Status> {
        let pb::UpdateQueueItemRequest {
            id,
            text,
            caller_pane,
        } = request.into_inner();
        self.may_write_queue(&caller_pane)?;
        schema::validate_queue_text(&text).map_err(|field| invalid(&field))?;
        self.store
            .update_queue_item(&id, &text, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        self.queue_changed(None).await?;
        Ok(Response::new(pb::UpdateQueueItemResponse {}))
    }

    async fn done_queue_item(
        &self,
        request: Request<pb::DoneQueueItemRequest>,
    ) -> Result<Response<pb::DoneQueueItemResponse>, Status> {
        let pb::DoneQueueItemRequest { id, caller_pane } = request.into_inner();
        self.may_write_queue(&caller_pane)?;
        self.store
            .done_queue_item(&id, now_ms())
            .await
            .map_err(unavailable)?
            .map_err(refused)?;
        self.queue_changed(None).await?;
        Ok(Response::new(pb::DoneQueueItemResponse {}))
    }

    async fn set_queue_note(
        &self,
        request: Request<pb::SetQueueNoteRequest>,
    ) -> Result<Response<pb::SetQueueNoteResponse>, Status> {
        let pb::SetQueueNoteRequest { text, caller_pane } = request.into_inner();
        self.may_write_queue(&caller_pane)?;
        schema::validate_queue_text(&text).map_err(|field| invalid(&field))?;
        self.store
            .set_queue_note(&text)
            .await
            .map_err(unavailable)?;
        self.queue_changed(None).await?;
        Ok(Response::new(pb::SetQueueNoteResponse {}))
    }

    async fn list_queue(
        &self,
        request: Request<pb::ListQueueRequest>,
    ) -> Result<Response<pb::ListQueueResponse>, Status> {
        let done_since_ms = request.into_inner().done_since_ms;
        let listing = self
            .store
            .list_queue(done_since_ms)
            .await
            .map_err(unavailable)?;
        Ok(Response::new(pb::ListQueueResponse {
            now: listing.now.map(queue_item_to_proto),
            parked: listing
                .parked
                .into_iter()
                .map(queue_item_to_proto)
                .collect(),
            done: listing.done.into_iter().map(queue_item_to_proto).collect(),
            note: listing.note,
        }))
    }

    async fn import(
        &self,
        request: Request<pb::ImportRequest>,
    ) -> Result<Response<pb::ImportResponse>, Status> {
        let pb::ImportRequest {
            product,
            queue,
            markdown,
            apply,
            caller_pane,
            utc_offset_seconds,
        } = request.into_inner();
        check_target(queue, &product)?;
        // An applied import is a write; a dry run is a read of a known product.
        match (queue, apply) {
            (true, true) => self.may_write_queue(&caller_pane)?,
            (false, true) => self.may_write_product(&caller_pane, &product)?,
            (true, false) => {}
            (false, false) => self.guard.product_known(&product).map_err(denied)?,
        }
        let now = now_ms();
        let counts = if queue {
            let import = markdown::parse_queue(&markdown).map_err(unparsed)?;
            let done_at = done_at_ms(import.date, utc_offset_seconds)?;
            if !self.store.queue_is_empty().await.map_err(unavailable)? {
                return Err(refused(Refusal::NotEmpty));
            }
            let counts = import.counts();
            if apply {
                self.store
                    .import_queue(import, done_at, now)
                    .await
                    .map_err(unavailable)?
                    .map_err(refused)?;
                let count = counts.now + counts.parked + counts.done;
                info!(tool = NAME, count, "backlog imported");
            }
            counts
        } else {
            let import = markdown::parse_backlog(&markdown).map_err(unparsed)?;
            if !self
                .store
                .product_is_empty(&product)
                .await
                .map_err(unavailable)?
            {
                return Err(refused(Refusal::NotEmpty));
            }
            let counts = import.counts();
            if apply {
                self.store
                    .import_backlog(&product, import, now)
                    .await
                    .map_err(unavailable)?
                    .map_err(refused)?;
                let count =
                    counts.items + counts.rule + counts.history + counts.held + counts.left_out;
                info!(tool = NAME, product, count, "backlog imported");
            }
            counts
        };
        Ok(Response::new(pb::ImportResponse {
            counts: Some(counts_to_proto(counts)),
            applied: apply,
        }))
    }

    async fn export(
        &self,
        request: Request<pb::ExportRequest>,
    ) -> Result<Response<pb::ExportResponse>, Status> {
        let pb::ExportRequest { product, queue } = request.into_inner();
        check_target(queue, &product)?;
        let markdown = if queue {
            let listing = self.store.list_queue(0).await.map_err(unavailable)?;
            markdown::export_queue(&listing)
        } else {
            let items = self.store.list_items(&product).await.map_err(unavailable)?;
            let notes = self
                .store
                .list_notes(&product, None, None)
                .await
                .map_err(unavailable)?;
            markdown::export_backlog(&product, &items, &notes)
        };
        Ok(Response::new(pb::ExportResponse { markdown }))
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;

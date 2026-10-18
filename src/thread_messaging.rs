// Adapted from deno_runtime 0.267.0 ops/worker_host.rs, Copyright 2018-2026 the Deno authors, MIT license.
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use deno_core::{
    op2, CancelFuture, CancelHandle, DetachedBuffer, OpState, RcRef, Resource, ResourceId,
};
use deno_runtime::deno_web::{
    deserialize_js_transferables, serialize_transferables, JsMessageData, MessagePortError,
    Transferable,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

type ThreadMessage = (DetachedBuffer, Vec<Transferable>);

struct ThreadRegistryEntry {
    sender: UnboundedSender<ThreadMessage>,
    listener_count: AtomicU32,
}

#[derive(Clone, Default)]
pub(crate) struct ThreadMessagingDomain(Arc<Mutex<HashMap<u32, Arc<ThreadRegistryEntry>>>>);

impl ThreadMessagingDomain {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn register(
        &self,
        thread_id: u32,
    ) -> (Arc<ThreadRegistryEntry>, UnboundedReceiver<ThreadMessage>) {
        let (sender, receiver) = unbounded_channel();
        let entry = Arc::new(ThreadRegistryEntry {
            sender,
            listener_count: AtomicU32::new(0),
        });
        self.0.lock().unwrap().insert(thread_id, entry.clone());
        (entry, receiver)
    }

    fn unregister(&self, thread_id: u32, expected: &Arc<ThreadRegistryEntry>) {
        let mut entries = self.0.lock().unwrap();
        if entries
            .get(&thread_id)
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            entries.remove(&thread_id);
        }
    }
}

pub(crate) fn inject_domain(state: &mut OpState, domain: ThreadMessagingDomain) {
    state.put(domain);
}

struct ThreadMessageReceiver {
    thread_id: u32,
    domain: ThreadMessagingDomain,
    entry: Arc<ThreadRegistryEntry>,
    rx: RefCell<UnboundedReceiver<ThreadMessage>>,
    cancel: CancelHandle,
}

impl ThreadMessageReceiver {
    fn unregister(&self) {
        self.domain.unregister(self.thread_id, &self.entry);
    }
}

impl Resource for ThreadMessageReceiver {
    fn name(&self) -> std::borrow::Cow<'_, str> {
        "threadMessageReceiver".into()
    }

    fn close(self: Rc<Self>) {
        self.cancel.cancel();
        self.unregister();
    }
}

impl Drop for ThreadMessageReceiver {
    fn drop(&mut self) {
        self.unregister();
    }
}

#[op2(fast)]
#[smi]
fn op_node_worker_thread_register(state: &mut OpState, thread_id: u32) -> ResourceId {
    let domain = state.borrow::<ThreadMessagingDomain>().clone();
    let (entry, rx) = domain.register(thread_id);
    state.resource_table.add(ThreadMessageReceiver {
        thread_id,
        domain,
        entry,
        rx: RefCell::new(rx),
        cancel: CancelHandle::default(),
    })
}

#[op2(fast)]
fn op_node_worker_thread_set_listener_count(state: &mut OpState, thread_id: u32, count: u32) {
    if let Some(entry) = state
        .borrow::<ThreadMessagingDomain>()
        .0
        .lock()
        .unwrap()
        .get(&thread_id)
    {
        entry.listener_count.store(count, Ordering::SeqCst);
    }
}

#[op2]
fn op_node_worker_thread_post_message(
    state: &mut OpState,
    target_thread_id: u32,
    #[serde] data: JsMessageData,
    force: bool,
) -> Result<u8, MessagePortError> {
    let domain = state.borrow::<ThreadMessagingDomain>().clone();
    let entry = match domain.0.lock().unwrap().get(&target_thread_id) {
        Some(entry) => entry.clone(),
        None => return Ok(0),
    };
    if !force && entry.listener_count.load(Ordering::SeqCst) == 0 {
        return Ok(1);
    }
    let transferables = if data.transferables.is_empty() {
        vec![]
    } else {
        deserialize_js_transferables(state, data.transferables)?
    };
    if entry.sender.send((data.data, transferables)).is_err() {
        return Ok(0);
    }
    Ok(2)
}

#[op2]
async fn op_node_worker_thread_recv_message(
    state: Rc<RefCell<OpState>>,
    #[smi] rid: ResourceId,
) -> Result<Option<JsMessageData>, MessagePortError> {
    let resource = match state
        .borrow()
        .resource_table
        .get::<ThreadMessageReceiver>(rid)
    {
        Ok(resource) => resource,
        Err(_) => return Ok(None),
    };
    let cancel = RcRef::map(resource.clone(), |resource| &resource.cancel);
    let recv = poll_fn(|cx| resource.rx.borrow_mut().poll_recv(cx));
    let message = match recv.or_cancel(cancel).await {
        Ok(Some(message)) => message,
        Ok(None) | Err(_) => return Ok(None),
    };
    let (data, transferables) = message;
    let transferables = if transferables.is_empty() {
        vec![]
    } else {
        serialize_transferables(&mut state.borrow_mut(), transferables)
    };
    Ok(Some(JsMessageData {
        data,
        transferables,
    }))
}

deno_core::extension!(
    thread_messaging_ops,
    ops = [
        op_node_worker_thread_register,
        op_node_worker_thread_set_listener_count,
        op_node_worker_thread_post_message,
        op_node_worker_thread_recv_message,
    ]
);

/// Declarations in upstream name order, ready for runtime extension middleware replacement.
pub(crate) fn replacement_op_decls() -> Vec<deno_core::OpDecl> {
    thread_messaging_ops::init().ops.into_owned()
}

#[cfg(test)]
#[path = "thread_messaging/tests.rs"]
mod tests;

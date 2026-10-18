use super::*;

fn receiver(
    domain: &ThreadMessagingDomain,
    thread_id: u32,
) -> (Rc<ThreadMessageReceiver>, Arc<ThreadRegistryEntry>) {
    let (entry, rx) = domain.register(thread_id);
    let resource = Rc::new(ThreadMessageReceiver {
        thread_id,
        domain: domain.clone(),
        entry: entry.clone(),
        rx: RefCell::new(rx),
        cancel: CancelHandle::default(),
    });
    (resource, entry)
}

#[test]
fn domains_are_isolated_and_share_channel_state_internally() {
    let first = ThreadMessagingDomain::new();
    let second = ThreadMessagingDomain::new();
    let (first_resource, first_entry) = receiver(&first, 0);
    let (second_resource, second_entry) = receiver(&second, 0);
    assert!(!Arc::ptr_eq(&first_entry, &second_entry));
    first_entry.listener_count.store(1, Ordering::SeqCst);
    assert_eq!(second_entry.listener_count.load(Ordering::SeqCst), 0);
    assert!(!first_entry.sender.is_closed());
    drop(first_resource);
    assert!(first_entry.sender.is_closed());
    assert!(!second_entry.sender.is_closed());
    drop(second_resource);
}

#[test]
fn stale_resource_drop_cannot_unregister_replacement() {
    let domain = ThreadMessagingDomain::new();
    let (old_resource, old_entry) = receiver(&domain, 0);
    let (new_resource, new_entry) = receiver(&domain, 0);
    assert!(!Arc::ptr_eq(&old_entry, &new_entry));
    drop(old_resource);
    assert!(Arc::ptr_eq(
        domain.0.lock().unwrap().get(&0).unwrap(),
        &new_entry
    ));
    <ThreadMessageReceiver as Resource>::close(new_resource.clone());
    drop(new_resource);
    assert!(!domain.0.lock().unwrap().contains_key(&0));
}

#[test]
fn exposes_the_four_upstream_op_declarations() {
    let names: Vec<_> = replacement_op_decls()
        .into_iter()
        .map(|op| op.name)
        .collect();
    assert_eq!(
        names,
        [
            "op_node_worker_thread_register",
            "op_node_worker_thread_set_listener_count",
            "op_node_worker_thread_post_message",
            "op_node_worker_thread_recv_message",
        ]
    );
}

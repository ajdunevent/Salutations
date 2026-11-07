use std::cell::{Cell, OnceCell};

use gtk::{glib, prelude::*, subclass::prelude::*};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A message that is not part of a multi-message thread.
    Single,
    /// Stands for a whole thread; never holds the selection.
    Summary,
    /// A message inside an expanded thread.
    Child,
}

#[derive(Debug, Clone)]
pub struct Person {
    pub name: String,
    pub email: String,
}

impl Person {
    pub fn display(&self) -> &str {
        if self.name.is_empty() { &self.email } else { &self.name }
    }
}

#[derive(Debug, Clone)]
pub struct RowData {
    pub kind: RowKind,
    /// The thread's key (its root UID); the UID itself for a Single row.
    pub thread: String,
    /// None for Summary rows.
    pub uid: Option<String>,
    pub sender: String,
    /// Newest first, without the account's own address. Summary rows only.
    pub participants: Vec<Person>,
    pub date: i64,
    pub subject: String,
    pub preview: String,
    pub count: usize,
    pub attachment: bool,
    /// Marked high importance; for a Summary row, any message in it.
    pub important: bool,
    /// Search results from other folders: where the message is.
    pub location: Option<String>,
    /// The sender's domain when the message passed verification for it;
    /// the outer None when the summary did not say.
    pub verified: Option<Option<String>>,
    /// Summary rows: for each participant, the domain whose logo it may
    /// show, which needs every message of theirs in the thread verified
    /// for that domain.
    pub logos: Vec<Option<String>>,
}

mod imp {
    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::MessageObject)]
    pub struct MessageObject {
        #[property(get, set)]
        pub unread: Cell<bool>,
        #[property(get, set)]
        pub starred: Cell<bool>,
        /// Replied to; for a summary row, any message in the thread.
        #[property(get, set)]
        pub answered: Cell<bool>,
        /// Forwarded (the `$Forwarded` keyword); as `answered` for threads.
        #[property(get, set)]
        pub forwarded: Cell<bool>,
        /// Selection mode: the message is checked, or for a summary row,
        /// every message in the thread is.
        #[property(get, set)]
        pub checked: Cell<bool>,
        /// Summary rows: some but not all of the thread's messages are
        /// checked.
        #[property(get, set)]
        pub inconsistent: Cell<bool>,
        pub data: OnceCell<RowData>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MessageObject {
        const NAME: &'static str = "SalutationsMessageObject";
        type Type = super::MessageObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for MessageObject {}
}

glib::wrapper! {
    /// One row of the message list.
    pub struct MessageObject(ObjectSubclass<imp::MessageObject>);
}

impl MessageObject {
    pub fn new(data: RowData, unread: bool, starred: bool) -> Self {
        let obj: Self = glib::Object::builder()
            .property("unread", unread)
            .property("starred", starred)
            .build();
        let _ = obj.imp().data.set(data);
        obj
    }

    /// Sets the replied and forwarded marks.
    pub fn responded(self, answered: bool, forwarded: bool) -> Self {
        self.set_answered(answered);
        self.set_forwarded(forwarded);
        self
    }

    pub fn data(&self) -> &RowData {
        self.imp().data.get().expect("data is set in MessageObject::new")
    }
}

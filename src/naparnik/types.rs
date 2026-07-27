//! Upstream request and response types.

#[derive(Debug, PartialEq, Eq)]
pub struct Conversation {
    id: String,
    parent_uuid: Option<String>,
}

impl Conversation {
    #[must_use]
    pub fn new(id: String, parent_uuid: Option<String>) -> Self {
        Self { id, parent_uuid }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn parent_uuid(&self) -> Option<&str> {
        self.parent_uuid.as_deref()
    }

    pub fn advance_parent(&mut self, assistant_message_uuid: String) {
        self.parent_uuid = Some(assistant_message_uuid);
    }
}

#[cfg(test)]
mod tests {
    use super::Conversation;

    #[test]
    fn conversation_parent_starts_at_root_and_advances_to_assistant_message() {
        let mut conversation =
            Conversation::new("conversation-1".to_owned(), Some("root-1".to_owned()));

        assert_eq!(conversation.id(), "conversation-1");
        assert_eq!(conversation.parent_uuid(), Some("root-1"));

        conversation.advance_parent("assistant-1".to_owned());
        assert_eq!(conversation.parent_uuid(), Some("assistant-1"));
    }

    #[test]
    fn conversation_can_start_without_root_parent() {
        let conversation = Conversation::new("conversation-1".to_owned(), None);

        assert_eq!(conversation.parent_uuid(), None);
    }
}

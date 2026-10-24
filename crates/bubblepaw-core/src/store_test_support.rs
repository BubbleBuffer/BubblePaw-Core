use super::*;

impl Store {
    pub(crate) fn append(&self, message: &Message) -> Result<()> {
        self.append_batch(std::slice::from_ref(message))
    }
}

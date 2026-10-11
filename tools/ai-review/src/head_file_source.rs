/// Port to the content of a changed file at the pull request head.
pub trait HeadFileSource: Sync {
    /// Returns the file content at the head, or `None` when the path does not
    /// exist there.
    fn read(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<String>>> + Send;
}

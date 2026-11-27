use zed_extension_api as zed;

struct StarExtension;

impl zed::Extension for StarExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        _worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        Ok(zed::Command {
            command: "star".to_owned(),
            args: vec!["lsp".to_owned()],
            env: Default::default(),
        })
    }
}

zed::register_extension!(StarExtension);

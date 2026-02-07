/// A plugin that can extend the DbFlow engine.
pub trait Plugin: Send + Sync {
    /// The name of this plugin.
    fn name(&self) -> &str;

    /// Called during plugin registration to allow the plugin to set up its capabilities.
    fn setup(&self, ctx: &mut PluginContext);
}

/// Context passed to plugins during setup, allowing them to register capabilities.
pub struct PluginContext {
    registered: Vec<String>,
}

impl PluginContext {
    pub fn new() -> Self {
        Self {
            registered: Vec::new(),
        }
    }

    /// Register a plugin by name.
    pub fn register(&mut self, name: &str) {
        self.registered.push(name.to_string());
    }

    /// Return the list of registered plugin names.
    pub fn registered_plugins(&self) -> &[String] {
        &self.registered
    }
}

impl Default for PluginContext {
    fn default() -> Self {
        Self::new()
    }
}

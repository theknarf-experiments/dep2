use dbflow_plugin::{Plugin, PluginContext};

pub struct PostgresPlugin;

impl Plugin for PostgresPlugin {
    fn name(&self) -> &str {
        "postgres"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
    }
}

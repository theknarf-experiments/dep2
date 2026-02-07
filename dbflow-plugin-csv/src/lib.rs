use dbflow_plugin::{Plugin, PluginContext};

pub struct CsvPlugin;

impl Plugin for CsvPlugin {
    fn name(&self) -> &str {
        "csv"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
    }
}

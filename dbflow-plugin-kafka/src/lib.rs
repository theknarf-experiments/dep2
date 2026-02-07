use dbflow_plugin::{Plugin, PluginContext};

pub struct KafkaPlugin;

impl Plugin for KafkaPlugin {
    fn name(&self) -> &str {
        "kafka"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
    }
}

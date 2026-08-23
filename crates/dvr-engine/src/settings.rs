use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::settings::UserSettings;

pub fn make_settings(name: &str, email: &str) -> anyhow::Result<UserSettings> {
    let mut config = StackedConfig::with_defaults();
    let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer.set_value("user.name", name)?;
    layer.set_value("user.email", email)?;
    config.add_layer(layer);
    Ok(UserSettings::from_config(config)?)
}

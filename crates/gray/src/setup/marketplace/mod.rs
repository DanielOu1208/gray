mod draw;
mod model;

#[derive(Clone, Debug, Default)]
pub(crate) struct ConsentView {
    pub capabilities: Vec<crate::plugin_install::policy::CapabilityLine>,
    pub commands: Vec<String>,
    pub scan_text: String,
}

#[cfg(test)]
mod tests;

use std::sync::Arc;

use gray::profile::from_plugins;

struct Stub(&'static str);

impl gray_plugin::Plugin for Stub {
    fn manifest(&self) -> gray_plugin::Manifest {
        gray_plugin::Manifest {
            name: self.0.to_string(),
            ..gray_plugin::Manifest::default()
        }
    }
}

#[test]
fn registry_from_plugins_collects_in_order() {
    let plugins: Vec<Arc<dyn gray_plugin::Plugin>> =
        vec![Arc::new(Stub("one")), Arc::new(Stub("two"))];
    let (reg, manifests) = from_plugins(&plugins);

    assert_eq!(reg.tool_names(), vec!["bash"]);
    assert_eq!(manifests.len(), 2);
    assert_eq!(manifests[0].name, "one");
    assert_eq!(manifests[1].name, "two");
}

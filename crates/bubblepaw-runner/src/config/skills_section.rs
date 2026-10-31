//! `[skills]`: the catalog switch, extra directories and per-skill toggles.
use super::*;

config_section! {
    /// Skill directories add collections after project and user skills. A
    /// `[skills.<name>]` table with `enabled = false` leaves one skill out of the catalog.
    #[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
    #[serde(default)]
    pub struct SkillsConfig {
        pub enabled: bool => LIVE,
        pub dirs: Vec<PathBuf> => LIVE,
        /// Per-skill tables; their schema is the open arm of [`SKILLS`].
        #[serde(flatten)]
        pub toggles: BTreeMap<String, SkillToggle> => Node::Flatten(&[]),
    }
}
config_section! {
    /// One skill's switch; a skill without a table is on.
    #[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct SkillToggle {
        #[serde(default = "on")]
        pub enabled: bool => LIVE,
    }
}
fn on() -> bool {
    true
}
/// The `skills` table: its fixed keys and one table per skill name.
pub(super) const SKILLS: Node = Node::Open(SkillsConfig::FIELDS, &Node::Table(SkillToggle::FIELDS));

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dirs: Vec::new(),
            toggles: BTreeMap::new(),
        }
    }
}
impl SkillsConfig {
    /// Names of the skills a `[skills.<name>]` table turned off.
    pub fn disabled(&self) -> BTreeSet<String> {
        self.toggles
            .iter()
            .filter(|(_, toggle)| !toggle.enabled)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

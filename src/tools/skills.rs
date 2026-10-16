//! Explicit Skill activation and package-resource tools.

use super::ToolHandler;
use crate::model::ToolSpec;
use crate::skills::SkillTurnState;

const ACTIVATE_DESCRIPTION: &str = "Load one Skill from the current catalog snapshot (complete SKILL.md limit: 64 KiB; combined loaded Skill content limit: 256 KiB per turn). The instructions become available on the next provider request in this turn; the tool result contains only an acknowledgement.";
const RESOURCE_DESCRIPTION: &str = "Load one normalized relative text resource of at most 64 KiB from an already activated Skill (combined loaded Skill content limit: 256 KiB per turn). Its content becomes available on the next provider request in this turn; the tool result contains only an acknowledgement.";

pub(crate) struct ActivateSkillTool {
    state: SkillTurnState,
}

impl ActivateSkillTool {
    pub(crate) fn new(state: SkillTurnState) -> Self {
        Self { state }
    }

    pub(crate) fn spec() -> ToolSpec {
        ToolSpec::new(
            "activate_skill",
            ACTIVATE_DESCRIPTION,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "snapshot_id": {
                        "type": "string",
                        "description": "The snapshot_id shown in the current Skills catalog"
                    },
                    "name": {
                        "type": "string",
                        "description": "The exact Skill name from the current catalog"
                    }
                },
                "required": ["snapshot_id", "name"]
            }),
        )
    }
}

impl ToolHandler for ActivateSkillTool {
    fn call(&self, input: &serde_json::Value) -> Result<String, String> {
        let snapshot_id = input["snapshot_id"]
            .as_str()
            .ok_or("activate_skill: `snapshot_id` must be a string")?;
        let name = input["name"]
            .as_str()
            .ok_or("activate_skill: `name` must be a string")?;
        let result = self.state.activate(snapshot_id, name);
        match &result {
            Ok(_) => eprintln!("skills activation: loaded '{name}' for this turn"),
            Err(error) => eprintln!("skills activation failed: {error}"),
        }
        result
    }
}

pub(crate) struct ReadSkillResourceTool {
    state: SkillTurnState,
}

impl ReadSkillResourceTool {
    pub(crate) fn new(state: SkillTurnState) -> Self {
        Self { state }
    }

    pub(crate) fn spec() -> ToolSpec {
        ToolSpec::new(
            "read_skill_resource",
            RESOURCE_DESCRIPTION,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "snapshot_id": {
                        "type": "string",
                        "description": "The snapshot_id shown in the current Skills catalog"
                    },
                    "name": {
                        "type": "string",
                        "description": "The exact Skill name from the current catalog"
                    },
                    "relative_path": {
                        "type": "string",
                        "description": "A normalized relative text path inside the selected Skill package"
                    }
                },
                "required": ["snapshot_id", "name", "relative_path"]
            }),
        )
    }
}

impl ToolHandler for ReadSkillResourceTool {
    fn call(&self, input: &serde_json::Value) -> Result<String, String> {
        let snapshot_id = input["snapshot_id"]
            .as_str()
            .ok_or("read_skill_resource: `snapshot_id` must be a string")?;
        let name = input["name"]
            .as_str()
            .ok_or("read_skill_resource: `name` must be a string")?;
        let relative_path = input["relative_path"]
            .as_str()
            .ok_or("read_skill_resource: `relative_path` must be a string")?;
        let result = self.state.read_resource(snapshot_id, name, relative_path);
        match &result {
            Ok(_) => eprintln!("skills resource: loaded for '{name}' in this turn"),
            Err(error) => eprintln!("skills resource failed: {error}"),
        }
        result
    }
}

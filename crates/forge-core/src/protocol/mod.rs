pub const TOOL_NAMESPACE_SEP: &str = "__";

pub fn namespace_tool(server: &str, tool: &str) -> String {
    format!("{}{}{}", server, TOOL_NAMESPACE_SEP, tool)
}

pub fn parse_namespaced_tool(namespaced: &str) -> Option<(&str, &str)> {
    namespaced.split_once(TOOL_NAMESPACE_SEP)
}

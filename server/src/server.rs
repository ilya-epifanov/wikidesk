use std::borrow::Cow;
use std::sync::Arc;

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, ListToolsResult, ServerCapabilities, ServerInfo,
        Tool,
    },
    schemars,
    service::RequestContext,
    tool, tool_router,
};

use crate::queue::TaskStatus;
use crate::surface::{ResearchSurface, SurfaceError};
use crate::wiki_instance::WikiInstance;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ResearchParams {
    /// The research question to investigate
    pub question: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetResultParams {
    /// The task ID returned by the research tool
    pub task_id: String,
    /// Optional local mirror path used when rendering wikilinks
    pub local_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResearchServer {
    state: Arc<WikiInstance>,
    tool_router: ToolRouter<Self>,
}

impl ResearchServer {
    pub fn new(state: Arc<WikiInstance>) -> Self {
        let mut tool_router = Self::tool_router();
        if let Some(route) = tool_router.map.get_mut("research") {
            let base = route.attr.description.as_deref().unwrap_or_default();
            let desc = state.config.research_tool_description();
            route.attr.description = Some(Cow::Owned(format!("{base}\n\n{desc}")));
        }
        Self { state, tool_router }
    }
}

#[tool_router]
impl ResearchServer {
    /// Submit a research question to be investigated against the wiki.
    /// Returns a task_id to poll with get_result.
    #[tool(name = "research")]
    async fn research(
        &self,
        Parameters(params): Parameters<ResearchParams>,
    ) -> Result<String, ErrorData> {
        let task_id = ResearchSurface::new(self.state.clone())
            .submit_research(params.question)
            .await
            .map_err(|_| ErrorData::internal_error("research queue is full", None))?;
        Ok(serde_json::json!({ "task_id": task_id }).to_string())
    }

    /// Get the status and result of a research task.
    #[tool(name = "get_result")]
    async fn get_result(
        &self,
        Parameters(params): Parameters<GetResultParams>,
    ) -> Result<String, ErrorData> {
        match ResearchSurface::new(self.state.clone())
            .poll_result(&params.task_id, params.local_path)
            .await
            .map_err(mcp_surface_error)?
        {
            Some(TaskStatus::Failed { error }) => Err(mcp_research_failed(error)),
            Some(result) => Ok(serde_json::to_string(&result).unwrap()),
            None => Err(ErrorData::resource_not_found(
                format!("unknown task_id '{}'", params.task_id),
                None,
            )),
        }
    }
}

fn mcp_surface_error(err: SurfaceError) -> ErrorData {
    match err {
        SurfaceError::InvalidLocalPath(error) => {
            ErrorData::invalid_params(format!("invalid local_path: {error}"), None)
        }
        other => ErrorData::internal_error(other.to_string(), None),
    }
}

fn mcp_research_failed(error: String) -> ErrorData {
    ErrorData::internal_error(format!("research failed: {error}"), None)
}

impl ServerHandler for ResearchServer {
    fn get_info(&self) -> ServerInfo {
        let instructions = self.state.config.mcp_instructions();
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(instructions)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tool_context = ToolCallContext::new(self, request, context);
        self.tool_router.call(tool_context).await
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            ..Default::default()
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }
}

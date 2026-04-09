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

use crate::queue::{AppState, TaskStatus};
use crate::rewrite;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ResearchParams {
    /// The research question to investigate
    pub question: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetResultParams {
    /// The task ID returned by the research tool
    pub task_id: String,
}

#[derive(Debug, Clone)]
pub struct ResearchServer {
    state: Arc<AppState>,
    tool_router: ToolRouter<Self>,
}

impl ResearchServer {
    pub fn new(state: Arc<AppState>) -> Self {
        let mut tool_router = Self::tool_router();
        if let Some(desc) = &state.config.research_tool_description
            && let Some(route) = tool_router.map.get_mut("research")
        {
            let base = route.attr.description.as_deref().unwrap_or_default();
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
        let task_id = self
            .state
            .enqueue(params.question)
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
        match self.state.get_task_status(&params.task_id).await {
            Some(TaskStatus::Done { answer }) => {
                let wiki_repo = self.state.config.wiki_repo.clone();
                let answer = tokio::task::spawn_blocking(move || {
                    rewrite::rewrite_wikilinks(&answer, &wiki_repo, "wiki")
                })
                .await
                .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))?;
                Ok(serde_json::json!({ "status": "done", "answer": answer }).to_string())
            }
            Some(TaskStatus::Failed { error }) => Err(ErrorData::internal_error(
                format!("research failed: {error}"),
                None,
            )),
            Some(status) => Ok(serde_json::to_string(&status).unwrap()),
            None => Err(ErrorData::resource_not_found(
                format!("unknown task_id '{}'", params.task_id),
                None,
            )),
        }
    }
}

impl ServerHandler for ResearchServer {
    fn get_info(&self) -> ServerInfo {
        let instructions = self.state.config.instructions.as_deref().unwrap_or(
            "Research server: use 'research' to submit questions, 'get_result' to poll results.",
        );
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

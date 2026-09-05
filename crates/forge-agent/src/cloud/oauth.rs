/// Why a probe payload could not be mapped to a
/// [`forge_primitives::usage::UsageSnapshot`]. The zai mapper that
/// produces this lives in the forge-providers Zai backend now.
#[derive(Debug)]
pub enum OauthFetchError {
    Failed(String),
}

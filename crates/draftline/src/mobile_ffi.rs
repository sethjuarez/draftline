//! C ABI foundation for native mobile hosts.
//!
//! The functions in this module keep Draftline's Rust workspace/sync semantics as
//! the source of truth while exposing an opaque-handle API that Swift can wrap.
//! Structured Draftline values are returned as UTF-8 JSON strings so the ABI can
//! stay small and stable.

use std::{
    ffi::{CStr, CString},
    fs,
    os::raw::{c_char, c_void},
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    ptr, slice,
};

use crate::{
    path::normalize_workspace_relative, ContentPolicy, DraftlineError, PublishToken,
    RemoteCredential, RemoteCredentialRequest, RemoteOptions, Workspace,
};

type FfiResult<T> = std::result::Result<T, FfiFailure>;

/// Opaque workspace handle owned by Draftline and passed across the C ABI.
pub struct DraftlineMobileWorkspace {
    workspace: Workspace,
}

/// Stable status codes returned by mobile bridge functions.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftlineMobileStatusCode {
    Ok = 0,
    NullArgument = 1,
    InvalidUtf8 = 2,
    InvalidContentPolicy = 3,
    DraftlineError = 4,
    Panic = 5,
    CredentialRejected = 6,
}

/// C-safe status with an optional heap-allocated error message.
#[repr(C)]
#[derive(Debug)]
pub struct DraftlineMobileStatus {
    pub code: DraftlineMobileStatusCode,
    pub message: *mut c_char,
}

/// Result for functions that create or return a workspace handle.
#[repr(C)]
#[derive(Debug)]
pub struct DraftlineMobileWorkspaceResult {
    pub status: DraftlineMobileStatus,
    pub workspace: *mut DraftlineMobileWorkspace,
}

/// Result for functions that return a heap-allocated UTF-8 string.
#[repr(C)]
#[derive(Debug)]
pub struct DraftlineMobileStringResult {
    pub status: DraftlineMobileStatus,
    pub value: *mut c_char,
}

/// Host-owned content policy passed to open/clone.
#[repr(C)]
#[derive(Debug)]
pub struct DraftlineMobileContentPolicy {
    pub include_paths: *const *const c_char,
    pub include_path_count: usize,
    pub exclude_paths: *const *const c_char,
    pub exclude_path_count: usize,
    pub include_extensions: *const *const c_char,
    pub include_extension_count: usize,
    /// Zero keeps Draftline's default threshold.
    pub large_file_threshold_bytes: u64,
}

/// Credential kinds a mobile host can return to Draftline.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftlineMobileCredentialKind {
    Default = 0,
    UsernamePassword = 1,
    SshAgent = 2,
    SshKey = 3,
}

/// Credential request passed to the host callback.
#[repr(C)]
#[derive(Debug)]
pub struct DraftlineMobileCredentialRequest {
    pub url: *const c_char,
    pub username_from_url: *const c_char,
    pub allows_default: bool,
    pub allows_username_password: bool,
    pub allows_ssh_key: bool,
}

/// Credential material written by the host callback.
///
/// Pointers are borrowed only for the duration of the callback invocation.
#[repr(C)]
#[derive(Debug)]
pub struct DraftlineMobileCredential {
    pub kind: DraftlineMobileCredentialKind,
    pub username: *const c_char,
    pub password: *const c_char,
    pub public_key_path: *const c_char,
    pub private_key_path: *const c_char,
    pub passphrase: *const c_char,
}

pub type DraftlineMobileCredentialCallback = Option<
    unsafe extern "C" fn(
        request: *const DraftlineMobileCredentialRequest,
        credential_out: *mut DraftlineMobileCredential,
        user_data: *mut c_void,
    ) -> DraftlineMobileStatusCode,
>;

#[derive(Debug)]
struct FfiFailure {
    code: DraftlineMobileStatusCode,
    message: String,
}

impl FfiFailure {
    fn new(code: DraftlineMobileStatusCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<DraftlineError> for FfiFailure {
    fn from(error: DraftlineError) -> Self {
        Self::new(DraftlineMobileStatusCode::DraftlineError, error.to_string())
    }
}

impl From<serde_json::Error> for FfiFailure {
    fn from(error: serde_json::Error) -> Self {
        Self::new(DraftlineMobileStatusCode::DraftlineError, error.to_string())
    }
}

fn ok_status() -> DraftlineMobileStatus {
    DraftlineMobileStatus {
        code: DraftlineMobileStatusCode::Ok,
        message: ptr::null_mut(),
    }
}

fn error_status(error: FfiFailure) -> DraftlineMobileStatus {
    DraftlineMobileStatus {
        code: error.code,
        message: into_c_string(error.message),
    }
}

fn panic_status() -> DraftlineMobileStatus {
    error_status(FfiFailure::new(
        DraftlineMobileStatusCode::Panic,
        "Draftline mobile bridge operation panicked",
    ))
}

fn into_c_string(value: impl Into<String>) -> *mut c_char {
    let sanitized = value.into().replace('\0', " ");
    CString::new(sanitized)
        .expect("interior NULs were removed")
        .into_raw()
}

unsafe fn required_str<'a>(ptr: *const c_char, name: &str) -> FfiResult<&'a str> {
    if ptr.is_null() {
        return Err(FfiFailure::new(
            DraftlineMobileStatusCode::NullArgument,
            format!("{name} must not be null"),
        ));
    }

    CStr::from_ptr(ptr).to_str().map_err(|_| {
        FfiFailure::new(
            DraftlineMobileStatusCode::InvalidUtf8,
            format!("{name} must be valid UTF-8"),
        )
    })
}

unsafe fn optional_string(ptr: *const c_char, name: &str) -> FfiResult<Option<String>> {
    if ptr.is_null() {
        Ok(None)
    } else {
        required_str(ptr, name).map(|value| Some(value.to_string()))
    }
}

unsafe fn string_array(
    ptr: *const *const c_char,
    count: usize,
    name: &str,
) -> FfiResult<Vec<String>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err(FfiFailure::new(
            DraftlineMobileStatusCode::NullArgument,
            format!("{name} must not be null when count is non-zero"),
        ));
    }

    slice::from_raw_parts(ptr, count)
        .iter()
        .enumerate()
        .map(|(index, item)| required_str(*item, &format!("{name}[{index}]")).map(str::to_string))
        .collect()
}

unsafe fn content_policy_from_ptr(
    ptr: *const DraftlineMobileContentPolicy,
) -> FfiResult<ContentPolicy> {
    if ptr.is_null() {
        return Ok(ContentPolicy::default());
    }

    let policy = &*ptr;
    let include_paths = string_array(
        policy.include_paths,
        policy.include_path_count,
        "include_paths",
    )?;
    let exclude_paths = string_array(
        policy.exclude_paths,
        policy.exclude_path_count,
        "exclude_paths",
    )?;
    let include_extensions = string_array(
        policy.include_extensions,
        policy.include_extension_count,
        "include_extensions",
    )?;

    let mut content_policy = ContentPolicy::new()
        .include_paths(include_paths)
        .map_err(|error| {
            FfiFailure::new(
                DraftlineMobileStatusCode::InvalidContentPolicy,
                error.to_string(),
            )
        })?
        .exclude_paths(exclude_paths)
        .map_err(|error| {
            FfiFailure::new(
                DraftlineMobileStatusCode::InvalidContentPolicy,
                error.to_string(),
            )
        })?
        .include_extensions(include_extensions)
        .map_err(|error| {
            FfiFailure::new(
                DraftlineMobileStatusCode::InvalidContentPolicy,
                error.to_string(),
            )
        })?;

    if policy.large_file_threshold_bytes > 0 {
        content_policy =
            content_policy.with_large_file_threshold(policy.large_file_threshold_bytes);
    }

    Ok(content_policy)
}

unsafe fn workspace_from_handle<'a>(
    handle: *mut DraftlineMobileWorkspace,
) -> FfiResult<&'a mut DraftlineMobileWorkspace> {
    handle.as_mut().ok_or_else(|| {
        FfiFailure::new(
            DraftlineMobileStatusCode::NullArgument,
            "workspace handle must not be null",
        )
    })
}

fn ensure_tracked(workspace: &Workspace, path: &str) -> FfiResult<PathBuf> {
    let normalized = normalize_workspace_relative(path).map_err(FfiFailure::from)?;
    if !workspace
        .content_policy()
        .tracks(&normalized)
        .map_err(FfiFailure::from)?
    {
        return Err(FfiFailure::from(DraftlineError::PathOutsideContentPolicy(
            normalized,
        )));
    }
    Ok(normalized)
}

fn with_remote_options<'a>(
    callback: DraftlineMobileCredentialCallback,
    user_data: *mut c_void,
) -> RemoteOptions<'a> {
    if let Some(callback) = callback {
        RemoteOptions::new()
            .with_credentials(move |request| credential_from_callback(callback, user_data, request))
    } else {
        RemoteOptions::new()
    }
}

fn credential_from_callback(
    callback: unsafe extern "C" fn(
        *const DraftlineMobileCredentialRequest,
        *mut DraftlineMobileCredential,
        *mut c_void,
    ) -> DraftlineMobileStatusCode,
    user_data: *mut c_void,
    request: RemoteCredentialRequest<'_>,
) -> crate::Result<RemoteCredential> {
    let url = CString::new(request.url.replace('\0', " ")).expect("interior NULs were removed");
    let username = request
        .username_from_url
        .map(|value| CString::new(value.replace('\0', " ")).expect("interior NULs were removed"));
    let mobile_request = DraftlineMobileCredentialRequest {
        url: url.as_ptr(),
        username_from_url: username
            .as_ref()
            .map(|value| value.as_ptr())
            .unwrap_or(ptr::null()),
        allows_default: request.allows_default,
        allows_username_password: request.allows_username_password,
        allows_ssh_key: request.allows_ssh_key,
    };
    let mut credential = DraftlineMobileCredential {
        kind: DraftlineMobileCredentialKind::Default,
        username: ptr::null(),
        password: ptr::null(),
        public_key_path: ptr::null(),
        private_key_path: ptr::null(),
        passphrase: ptr::null(),
    };

    let status = unsafe { callback(&mobile_request, &mut credential, user_data) };
    if status != DraftlineMobileStatusCode::Ok {
        return Err(DraftlineError::InvalidContributorIdentity(format!(
            "credential callback failed with status {status:?}"
        )));
    }

    unsafe { mobile_credential_to_remote(credential) }.map_err(|error| {
        DraftlineError::InvalidContributorIdentity(format!(
            "credential callback returned invalid credential: {}",
            error.message
        ))
    })
}

unsafe fn mobile_credential_to_remote(
    credential: DraftlineMobileCredential,
) -> FfiResult<RemoteCredential> {
    match credential.kind {
        DraftlineMobileCredentialKind::Default => Ok(RemoteCredential::Default),
        DraftlineMobileCredentialKind::UsernamePassword => Ok(RemoteCredential::UsernamePassword {
            username: required_str(credential.username, "credential.username")?.to_string(),
            password: required_str(credential.password, "credential.password")?.to_string(),
        }),
        DraftlineMobileCredentialKind::SshAgent => Ok(RemoteCredential::SshAgent {
            username: required_str(credential.username, "credential.username")?.to_string(),
        }),
        DraftlineMobileCredentialKind::SshKey => Ok(RemoteCredential::SshKey {
            username: required_str(credential.username, "credential.username")?.to_string(),
            public_key: optional_string(credential.public_key_path, "credential.public_key_path")?
                .map(PathBuf::from),
            private_key: PathBuf::from(required_str(
                credential.private_key_path,
                "credential.private_key_path",
            )?),
            passphrase: optional_string(credential.passphrase, "credential.passphrase")?,
        }),
    }
}

fn workspace_result(result: FfiResult<Workspace>) -> DraftlineMobileWorkspaceResult {
    match result {
        Ok(workspace) => DraftlineMobileWorkspaceResult {
            status: ok_status(),
            workspace: Box::into_raw(Box::new(DraftlineMobileWorkspace { workspace })),
        },
        Err(error) => DraftlineMobileWorkspaceResult {
            status: error_status(error),
            workspace: ptr::null_mut(),
        },
    }
}

fn string_result(result: FfiResult<String>) -> DraftlineMobileStringResult {
    match result {
        Ok(value) => DraftlineMobileStringResult {
            status: ok_status(),
            value: into_c_string(value),
        },
        Err(error) => DraftlineMobileStringResult {
            status: error_status(error),
            value: ptr::null_mut(),
        },
    }
}

fn status_result(result: FfiResult<()>) -> DraftlineMobileStatus {
    match result {
        Ok(()) => ok_status(),
        Err(error) => error_status(error),
    }
}

/// Frees strings returned in `DraftlineMobileStatus.message` or
/// `DraftlineMobileStringResult.value`.
///
/// # Safety
///
/// `value` must be null or a pointer returned by Draftline mobile bridge string
/// allocation. Passing any other pointer is undefined behavior.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_string_free(value: *mut c_char) {
    if !value.is_null() {
        drop(CString::from_raw(value));
    }
}

/// Frees an opaque workspace handle returned by Draftline.
///
/// # Safety
///
/// `workspace` must be null or a pointer returned by a Draftline mobile bridge
/// workspace creation function. It must not be used after this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_free(workspace: *mut DraftlineMobileWorkspace) {
    if !workspace.is_null() {
        drop(Box::from_raw(workspace));
    }
}

/// Opens an existing workspace or initializes a new one at `path`.
///
/// # Safety
///
/// `path` must be a valid null-terminated UTF-8 string. `policy`, when non-null,
/// must point to a valid `DraftlineMobileContentPolicy` whose arrays remain valid
/// for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_open_or_init(
    path: *const c_char,
    policy: *const DraftlineMobileContentPolicy,
) -> DraftlineMobileWorkspaceResult {
    match catch_unwind(AssertUnwindSafe(|| {
        workspace_result((|| {
            let path = required_str(path, "path")?;
            let policy = content_policy_from_ptr(policy)?;
            Workspace::init_with_policy(path, policy).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileWorkspaceResult {
            status: panic_status(),
            workspace: ptr::null_mut(),
        },
    }
}

/// Clones a shared workspace from `remote_url` into `path`.
///
/// # Safety
///
/// String pointers must be valid null-terminated UTF-8. `policy`, when non-null,
/// and callback pointers must remain valid for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_clone(
    remote_url: *const c_char,
    path: *const c_char,
    policy: *const DraftlineMobileContentPolicy,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
) -> DraftlineMobileWorkspaceResult {
    match catch_unwind(AssertUnwindSafe(|| {
        workspace_result((|| {
            let remote_url = required_str(remote_url, "remote_url")?;
            let path = required_str(path, "path")?;
            let policy = content_policy_from_ptr(policy)?;
            let mut options = with_remote_options(credential_callback, credential_user_data);
            Workspace::clone_workspace_with_policy_and_options(
                remote_url,
                path,
                policy,
                &mut options,
            )
            .map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileWorkspaceResult {
            status: panic_status(),
            workspace: ptr::null_mut(),
        },
    }
}

/// Reads a policy-tracked UTF-8 file from the workspace.
///
/// # Safety
///
/// `workspace` must be a valid Draftline handle and `path` must be a valid
/// null-terminated UTF-8 workspace-relative path.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_read_file(
    workspace: *mut DraftlineMobileWorkspace,
    path: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let path = required_str(path, "path")?;
            let normalized = ensure_tracked(&handle.workspace, path)?;
            let resolved = handle
                .workspace
                .resolve_path(&normalized)
                .map_err(FfiFailure::from)?;
            let bytes = fs::read(resolved).map_err(DraftlineError::from)?;
            String::from_utf8(bytes).map_err(|_| {
                FfiFailure::new(
                    DraftlineMobileStatusCode::InvalidUtf8,
                    format!("workspace file {path} is not valid UTF-8"),
                )
            })
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Writes bytes to a policy-tracked workspace-relative file.
///
/// # Safety
///
/// `workspace` must be a valid Draftline handle. `path` must be valid UTF-8.
/// `content` must point to `content_len` readable bytes unless `content_len` is
/// zero.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_write_file(
    workspace: *mut DraftlineMobileWorkspace,
    path: *const c_char,
    content: *const u8,
    content_len: usize,
) -> DraftlineMobileStatus {
    match catch_unwind(AssertUnwindSafe(|| {
        status_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let path = required_str(path, "path")?;
            if content.is_null() && content_len > 0 {
                return Err(FfiFailure::new(
                    DraftlineMobileStatusCode::NullArgument,
                    "content must not be null when content_len is non-zero",
                ));
            }
            let normalized = ensure_tracked(&handle.workspace, path)?;
            let resolved = handle
                .workspace
                .resolve_path(&normalized)
                .map_err(FfiFailure::from)?;
            if let Some(parent) = resolved.parent() {
                fs::create_dir_all(parent).map_err(DraftlineError::from)?;
            }
            let bytes = slice::from_raw_parts(content, content_len);
            fs::write(resolved, bytes).map_err(DraftlineError::from)?;
            Ok(())
        })())
    })) {
        Ok(result) => result,
        Err(_) => panic_status(),
    }
}

/// Saves current policy-tracked changes as a Draftline version and returns JSON.
///
/// # Safety
///
/// `workspace` must be a valid Draftline handle and `label` must be valid
/// null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_save_version_json(
    workspace: *mut DraftlineMobileWorkspace,
    label: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let label = required_str(label, "label")?;
            let version = handle
                .workspace
                .save_version(label)
                .map_err(FfiFailure::from)?;
            serde_json::to_string(&version).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Returns local workspace diagnostics/status as JSON.
///
/// # Safety
///
/// `workspace` must be a valid Draftline handle.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_status_json(
    workspace: *mut DraftlineMobileWorkspace,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            handle.workspace.inspect_json().map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Fetches remote-tracking state without changing workspace files.
///
/// # Safety
///
/// `workspace` must be valid and `remote` must be valid null-terminated UTF-8.
/// Callback pointers must remain valid for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_fetch_remote(
    workspace: *mut DraftlineMobileWorkspace,
    remote: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
) -> DraftlineMobileStatus {
    match catch_unwind(AssertUnwindSafe(|| {
        status_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let remote = required_str(remote, "remote")?;
            let mut options = with_remote_options(credential_callback, credential_user_data);
            handle
                .workspace
                .fetch_remote_with_options(remote, &mut options)
                .map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => panic_status(),
    }
}

/// Returns current variation sync status for a fetched remote as JSON.
///
/// # Safety
///
/// `workspace` must be valid and `remote` must be valid null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_sync_status_json(
    workspace: *mut DraftlineMobileWorkspace,
    remote: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let remote = required_str(remote, "remote")?;
            let status = handle
                .workspace
                .sync_status(remote)
                .map_err(FfiFailure::from)?;
            serde_json::to_string(&status).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Returns apply-incoming preflight JSON using cached remote-tracking state.
///
/// # Safety
///
/// `workspace` must be valid and `remote` must be valid null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_preflight_apply_incoming_json(
    workspace: *mut DraftlineMobileWorkspace,
    remote: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let remote = required_str(remote, "remote")?;
            let report = handle
                .workspace
                .preflight_apply_incoming(remote)
                .map_err(FfiFailure::from)?;
            serde_json::to_string(&report).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Applies fast-forward incoming remote changes and returns result JSON.
///
/// # Safety
///
/// `workspace` must be valid and `remote` must be valid null-terminated UTF-8.
/// Callback pointers must remain valid for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_apply_incoming_json(
    workspace: *mut DraftlineMobileWorkspace,
    remote: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let remote = required_str(remote, "remote")?;
            let mut options = with_remote_options(credential_callback, credential_user_data);
            let result = handle
                .workspace
                .apply_incoming(remote, &mut options)
                .map_err(FfiFailure::from)?;
            serde_json::to_string(&result).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Preflights guarded publication and returns JSON containing the publish token.
///
/// # Safety
///
/// `workspace` must be valid and `remote` must be valid null-terminated UTF-8.
/// Callback pointers must remain valid for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_preflight_publish_json(
    workspace: *mut DraftlineMobileWorkspace,
    remote: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let remote = required_str(remote, "remote")?;
            let mut options = with_remote_options(credential_callback, credential_user_data);
            let preflight = handle
                .workspace
                .preflight_publish_with_options(remote, &mut options)
                .map_err(FfiFailure::from)?;
            serde_json::to_string(&preflight).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Publishes with a JSON token returned by preflight publish.
///
/// # Safety
///
/// `workspace` must be valid and `publish_token_json` must be valid
/// null-terminated UTF-8. Callback pointers must remain valid for this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_publish_json(
    workspace: *mut DraftlineMobileWorkspace,
    publish_token_json: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let publish_token_json = required_str(publish_token_json, "publish_token_json")?;
            let token: PublishToken =
                serde_json::from_str(publish_token_json).map_err(FfiFailure::from)?;
            let mut options = with_remote_options(credential_callback, credential_user_data);
            let result = handle
                .workspace
                .publish_with_options(token, &mut options)
                .map_err(FfiFailure::from)?;
            serde_json::to_string(&result).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    fn c_string(value: &str) -> CString {
        CString::new(value).unwrap()
    }

    unsafe fn result_to_string(result: DraftlineMobileStringResult) -> String {
        assert_eq!(result.status.code, DraftlineMobileStatusCode::Ok);
        assert!(result.status.message.is_null());
        let value = CStr::from_ptr(result.value).to_str().unwrap().to_string();
        draftline_mobile_string_free(result.value);
        value
    }

    #[test]
    fn mobile_bridge_opens_writes_reads_saves_and_reports_status() {
        let temp = tempfile::tempdir().unwrap();
        let path = c_string(temp.path().to_str().unwrap());
        let include = c_string("content");
        let includes = [include.as_ptr()];
        let policy = DraftlineMobileContentPolicy {
            include_paths: includes.as_ptr(),
            include_path_count: includes.len(),
            exclude_paths: ptr::null(),
            exclude_path_count: 0,
            include_extensions: ptr::null(),
            include_extension_count: 0,
            large_file_threshold_bytes: 0,
        };

        let opened = unsafe { draftline_mobile_workspace_open_or_init(path.as_ptr(), &policy) };
        assert_eq!(opened.status.code, DraftlineMobileStatusCode::Ok);
        assert!(!opened.workspace.is_null());

        let file_path = c_string("content/post.md");
        let content = b"Hello from mobile";
        let write_status = unsafe {
            draftline_mobile_workspace_write_file(
                opened.workspace,
                file_path.as_ptr(),
                content.as_ptr(),
                content.len(),
            )
        };
        assert_eq!(write_status.code, DraftlineMobileStatusCode::Ok);

        let read = unsafe {
            result_to_string(draftline_mobile_workspace_read_file(
                opened.workspace,
                file_path.as_ptr(),
            ))
        };
        assert_eq!(read, "Hello from mobile");

        let label = c_string("Mobile save");
        let version = unsafe {
            result_to_string(draftline_mobile_workspace_save_version_json(
                opened.workspace,
                label.as_ptr(),
            ))
        };
        assert!(version.contains("\"label\":\"Mobile save\""));

        let status =
            unsafe { result_to_string(draftline_mobile_workspace_status_json(opened.workspace)) };
        assert!(status.contains("\"current_variation\":\"main\""));

        unsafe { draftline_mobile_workspace_free(opened.workspace) };
    }

    #[test]
    fn mobile_bridge_rejects_paths_outside_host_policy() {
        let temp = tempfile::tempdir().unwrap();
        let path = c_string(temp.path().to_str().unwrap());
        let include = c_string("content");
        let includes = [include.as_ptr()];
        let policy = DraftlineMobileContentPolicy {
            include_paths: includes.as_ptr(),
            include_path_count: includes.len(),
            exclude_paths: ptr::null(),
            exclude_path_count: 0,
            include_extensions: ptr::null(),
            include_extension_count: 0,
            large_file_threshold_bytes: 0,
        };
        let opened = unsafe { draftline_mobile_workspace_open_or_init(path.as_ptr(), &policy) };
        assert_eq!(opened.status.code, DraftlineMobileStatusCode::Ok);

        let file_path = c_string("scratch/post.md");
        let content = b"blocked";
        let write_status = unsafe {
            draftline_mobile_workspace_write_file(
                opened.workspace,
                file_path.as_ptr(),
                content.as_ptr(),
                content.len(),
            )
        };

        assert_eq!(write_status.code, DraftlineMobileStatusCode::DraftlineError);
        let message = unsafe { CStr::from_ptr(write_status.message).to_str().unwrap() };
        assert!(message.contains("outside tracked content policy"));
        unsafe {
            draftline_mobile_string_free(write_status.message);
            draftline_mobile_workspace_free(opened.workspace);
        }
    }
}

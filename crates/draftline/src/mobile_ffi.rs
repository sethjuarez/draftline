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
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    path::normalize_workspace_relative, tauri_contract::merge_conflict_view_model, ContentPolicy,
    DraftlineError, MergeConflictResolution, MergeIncomingToken, PublishToken, RemoteCredential,
    RemoteCredentialRequest, RemoteOptions, Workspace,
};

type FfiResult<T> = std::result::Result<T, FfiFailure>;
const DEFAULT_MOBILE_REMOTE_TIMEOUT_MS: u64 = 20_000;

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
    RemoteTimeout = 7,
    RemoteNetwork = 8,
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
        let code = status_code_for_draftline_error(&error);
        Self::new(code, error.to_string())
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

fn status_code_for_draftline_error(error: &DraftlineError) -> DraftlineMobileStatusCode {
    match error {
        DraftlineError::RemoteOperationTimedOut { .. } => DraftlineMobileStatusCode::RemoteTimeout,
        DraftlineError::Git(git_error) if git_error.code() == git2::ErrorCode::Timeout => {
            DraftlineMobileStatusCode::RemoteTimeout
        }
        DraftlineError::InvalidContributorIdentity(_) => {
            DraftlineMobileStatusCode::CredentialRejected
        }
        DraftlineError::Git(git_error)
            if matches!(
                git_error.code(),
                git2::ErrorCode::Auth | git2::ErrorCode::Certificate
            ) =>
        {
            DraftlineMobileStatusCode::CredentialRejected
        }
        DraftlineError::Git(git_error)
            if matches!(
                git_error.class(),
                git2::ErrorClass::Net | git2::ErrorClass::Http | git2::ErrorClass::Ssh
            ) =>
        {
            DraftlineMobileStatusCode::RemoteNetwork
        }
        _ => DraftlineMobileStatusCode::DraftlineError,
    }
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
    with_remote_options_with_timeout(
        callback,
        user_data,
        Some(Duration::from_millis(DEFAULT_MOBILE_REMOTE_TIMEOUT_MS)),
    )
}

fn with_remote_options_with_timeout<'a>(
    callback: DraftlineMobileCredentialCallback,
    user_data: *mut c_void,
    timeout: Option<Duration>,
) -> RemoteOptions<'a> {
    let options = if let Some(callback) = callback {
        RemoteOptions::new()
            .with_credentials(move |request| credential_from_callback(callback, user_data, request))
    } else {
        RemoteOptions::new()
    };

    if let Some(timeout) = timeout {
        options.with_timeout(timeout)
    } else {
        options
    }
}

fn mobile_timeout(timeout_ms: u64) -> Option<Duration> {
    if timeout_ms == 0 {
        None
    } else {
        Some(Duration::from_millis(timeout_ms))
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MobileDeleteShelfResult {
    shelf_id: String,
    deleted: bool,
}

unsafe fn optional_paths_json(
    paths_json: *const c_char,
    name: &str,
) -> FfiResult<Option<Vec<PathBuf>>> {
    if paths_json.is_null() {
        return Ok(None);
    }

    let paths_json = required_str(paths_json, name)?;
    serde_json::from_str(paths_json)
        .map(Some)
        .map_err(FfiFailure::from)
}

fn dirty_paths(workspace: &Workspace) -> FfiResult<Vec<PathBuf>> {
    Ok(workspace
        .changed_files()
        .map_err(FfiFailure::from)?
        .into_iter()
        .map(|file| file.path)
        .collect())
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

/// Clones a shared workspace with an explicit native network timeout in milliseconds.
///
/// `timeout_ms == 0` disables Draftline's native network timeout for this call.
///
/// # Safety
///
/// String pointers must be valid null-terminated UTF-8. `policy`, when non-null,
/// and callback pointers must remain valid for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_clone_with_timeout(
    remote_url: *const c_char,
    path: *const c_char,
    policy: *const DraftlineMobileContentPolicy,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
    timeout_ms: u64,
) -> DraftlineMobileWorkspaceResult {
    match catch_unwind(AssertUnwindSafe(|| {
        workspace_result((|| {
            let remote_url = required_str(remote_url, "remote_url")?;
            let path = required_str(path, "path")?;
            let policy = content_policy_from_ptr(policy)?;
            let mut options = with_remote_options_with_timeout(
                credential_callback,
                credential_user_data,
                mobile_timeout(timeout_ms),
            );
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

/// Fetches remote-tracking state with an explicit native network timeout in milliseconds.
///
/// `timeout_ms == 0` disables Draftline's native network timeout for this call.
///
/// # Safety
///
/// `workspace` must be valid and `remote` must be valid null-terminated UTF-8.
/// Callback pointers must remain valid for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_fetch_remote_with_timeout(
    workspace: *mut DraftlineMobileWorkspace,
    remote: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
    timeout_ms: u64,
) -> DraftlineMobileStatus {
    match catch_unwind(AssertUnwindSafe(|| {
        status_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let remote = required_str(remote, "remote")?;
            let mut options = with_remote_options_with_timeout(
                credential_callback,
                credential_user_data,
                mobile_timeout(timeout_ms),
            );
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

/// Applies incoming changes with an explicit native network timeout in milliseconds.
///
/// `timeout_ms == 0` disables Draftline's native network timeout for this call.
///
/// # Safety
///
/// `workspace` must be valid and `remote` must be valid null-terminated UTF-8.
/// Callback pointers must remain valid for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_apply_incoming_json_with_timeout(
    workspace: *mut DraftlineMobileWorkspace,
    remote: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
    timeout_ms: u64,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let remote = required_str(remote, "remote")?;
            let mut options = with_remote_options_with_timeout(
                credential_callback,
                credential_user_data,
                mobile_timeout(timeout_ms),
            );
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

/// Preflights shelving selected policy-tracked files, or all dirty files when `paths_json` is null.
///
/// # Safety
///
/// `workspace` must be valid and `name` must be valid null-terminated UTF-8.
/// `paths_json`, when non-null, must be a valid UTF-8 JSON array of workspace-relative path strings.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_preflight_shelve_json(
    workspace: *mut DraftlineMobileWorkspace,
    name: *const c_char,
    paths_json: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let name = required_str(name, "name")?;
            let paths = match optional_paths_json(paths_json, "paths_json")? {
                Some(paths) => paths,
                None => dirty_paths(&handle.workspace)?,
            };
            let report = handle
                .workspace
                .preflight_shelve_files(name, paths)
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

/// Shelves selected policy-tracked files, or all dirty files when `paths_json` is null.
///
/// # Safety
///
/// `workspace` must be valid and `name` must be valid null-terminated UTF-8.
/// `paths_json`, when non-null, must be a valid UTF-8 JSON array of workspace-relative path strings.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_shelve_json(
    workspace: *mut DraftlineMobileWorkspace,
    name: *const c_char,
    paths_json: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let name = required_str(name, "name")?;
            let shelf = match optional_paths_json(paths_json, "paths_json")? {
                Some(paths) => handle.workspace.shelve_files(name, paths),
                None => handle
                    .workspace
                    .shelve_files(name, dirty_paths(&handle.workspace)?),
            }
            .map_err(FfiFailure::from)?;
            serde_json::to_string(&shelf).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Lists local shelves as JSON.
///
/// # Safety
///
/// `workspace` must be a valid Draftline handle.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_list_shelves_json(
    workspace: *mut DraftlineMobileWorkspace,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let shelves = handle.workspace.list_shelves().map_err(FfiFailure::from)?;
            serde_json::to_string(&shelves).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Previews a shelf as JSON without mutating the workspace.
///
/// # Safety
///
/// `workspace` must be valid and `shelf_id` must be valid null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_preview_shelf_json(
    workspace: *mut DraftlineMobileWorkspace,
    shelf_id: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let shelf_id = required_str(shelf_id, "shelf_id")?;
            let preview = handle
                .workspace
                .preview_shelf(shelf_id)
                .map_err(FfiFailure::from)?;
            serde_json::to_string(&preview).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Preflights applying a shelf as JSON without mutating the workspace.
///
/// # Safety
///
/// `workspace` must be valid and `shelf_id` must be valid null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_preflight_apply_shelf_json(
    workspace: *mut DraftlineMobileWorkspace,
    shelf_id: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let shelf_id = required_str(shelf_id, "shelf_id")?;
            let report = handle
                .workspace
                .preflight_apply_shelf(shelf_id)
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

/// Applies a shelf as workspace content, preserving the shelf, and returns JSON.
///
/// # Safety
///
/// `workspace` must be valid and `shelf_id` must be valid null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_apply_shelf_json(
    workspace: *mut DraftlineMobileWorkspace,
    shelf_id: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let shelf_id = required_str(shelf_id, "shelf_id")?;
            let shelf = handle
                .workspace
                .apply_shelf(shelf_id)
                .map_err(FfiFailure::from)?;
            serde_json::to_string(&shelf).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Deletes a shelf and returns JSON.
///
/// # Safety
///
/// `workspace` must be valid and `shelf_id` must be valid null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_delete_shelf_json(
    workspace: *mut DraftlineMobileWorkspace,
    shelf_id: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let shelf_id = required_str(shelf_id, "shelf_id")?;
            handle
                .workspace
                .delete_shelf(shelf_id)
                .map_err(FfiFailure::from)?;
            serde_json::to_string(&MobileDeleteShelfResult {
                shelf_id: shelf_id.to_string(),
                deleted: true,
            })
            .map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Returns merge-incoming preflight JSON using cached remote-tracking state.
///
/// # Safety
///
/// `workspace` must be valid and `remote` must be valid null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_preflight_merge_incoming_json(
    workspace: *mut DraftlineMobileWorkspace,
    remote: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let remote = required_str(remote, "remote")?;
            let report = handle
                .workspace
                .preflight_merge_incoming(remote)
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

/// Converts merge-incoming preflight JSON into grouped conflict-view JSON.
///
/// # Safety
///
/// `merge_report_json` must be a valid null-terminated UTF-8 JSON
/// `MergeIncomingReport` returned by Draftline.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_merge_conflict_view_model_json(
    merge_report_json: *const c_char,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let merge_report_json = required_str(merge_report_json, "merge_report_json")?;
            let report = serde_json::from_str(merge_report_json).map_err(FfiFailure::from)?;
            let view_model = merge_conflict_view_model(&report);
            serde_json::to_string(&view_model).map_err(FfiFailure::from)
        })())
    })) {
        Ok(result) => result,
        Err(_) => DraftlineMobileStringResult {
            status: panic_status(),
            value: ptr::null_mut(),
        },
    }
}

/// Writes a clean incoming merge using a preflight token and returns result JSON.
///
/// # Safety
///
/// `workspace` must be valid. String pointers must be valid null-terminated UTF-8.
/// Callback pointers must remain valid for this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_merge_incoming_json(
    workspace: *mut DraftlineMobileWorkspace,
    token_json: *const c_char,
    label: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let token_json = required_str(token_json, "token_json")?;
            let label = required_str(label, "label")?;
            let token: MergeIncomingToken =
                serde_json::from_str(token_json).map_err(FfiFailure::from)?;
            let mut options = with_remote_options(credential_callback, credential_user_data);
            let result = handle
                .workspace
                .merge_incoming(token, label, &mut options)
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

/// Writes a clean incoming merge with an explicit native network timeout in milliseconds.
///
/// `timeout_ms == 0` disables Draftline's native network timeout for this call.
///
/// # Safety
///
/// `workspace` must be valid. String pointers must be valid null-terminated UTF-8.
/// Callback pointers must remain valid for this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_merge_incoming_json_with_timeout(
    workspace: *mut DraftlineMobileWorkspace,
    token_json: *const c_char,
    label: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
    timeout_ms: u64,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let token_json = required_str(token_json, "token_json")?;
            let label = required_str(label, "label")?;
            let token: MergeIncomingToken =
                serde_json::from_str(token_json).map_err(FfiFailure::from)?;
            let mut options = with_remote_options_with_timeout(
                credential_callback,
                credential_user_data,
                mobile_timeout(timeout_ms),
            );
            let result = handle
                .workspace
                .merge_incoming(token, label, &mut options)
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

/// Writes an incoming merge with explicit resolution JSON and returns result JSON.
///
/// # Safety
///
/// `workspace` must be valid. String pointers must be valid null-terminated UTF-8.
/// `resolutions_json` must be a JSON array of `MergeConflictResolution` values.
/// Callback pointers must remain valid for this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_merge_incoming_with_resolutions_json(
    workspace: *mut DraftlineMobileWorkspace,
    token_json: *const c_char,
    label: *const c_char,
    resolutions_json: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let token_json = required_str(token_json, "token_json")?;
            let label = required_str(label, "label")?;
            let resolutions_json = required_str(resolutions_json, "resolutions_json")?;
            let token: MergeIncomingToken =
                serde_json::from_str(token_json).map_err(FfiFailure::from)?;
            let resolutions: Vec<MergeConflictResolution> =
                serde_json::from_str(resolutions_json).map_err(FfiFailure::from)?;
            let mut options = with_remote_options(credential_callback, credential_user_data);
            let result = handle
                .workspace
                .merge_incoming_with_resolutions(token, label, resolutions, &mut options)
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

/// Writes an incoming merge with explicit resolutions and a native network timeout.
///
/// `timeout_ms == 0` disables Draftline's native network timeout for this call.
///
/// # Safety
///
/// `workspace` must be valid. String pointers must be valid null-terminated UTF-8.
/// `resolutions_json` must be a JSON array of `MergeConflictResolution` values.
/// Callback pointers must remain valid for this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_merge_incoming_with_resolutions_json_with_timeout(
    workspace: *mut DraftlineMobileWorkspace,
    token_json: *const c_char,
    label: *const c_char,
    resolutions_json: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
    timeout_ms: u64,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let token_json = required_str(token_json, "token_json")?;
            let label = required_str(label, "label")?;
            let resolutions_json = required_str(resolutions_json, "resolutions_json")?;
            let token: MergeIncomingToken =
                serde_json::from_str(token_json).map_err(FfiFailure::from)?;
            let resolutions: Vec<MergeConflictResolution> =
                serde_json::from_str(resolutions_json).map_err(FfiFailure::from)?;
            let mut options = with_remote_options_with_timeout(
                credential_callback,
                credential_user_data,
                mobile_timeout(timeout_ms),
            );
            let result = handle
                .workspace
                .merge_incoming_with_resolutions(token, label, resolutions, &mut options)
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

/// Preflights guarded publication with an explicit native network timeout in milliseconds.
///
/// `timeout_ms == 0` disables Draftline's native network timeout for this call.
///
/// # Safety
///
/// `workspace` must be valid and `remote` must be valid null-terminated UTF-8.
/// Callback pointers must remain valid for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_preflight_publish_json_with_timeout(
    workspace: *mut DraftlineMobileWorkspace,
    remote: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
    timeout_ms: u64,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let remote = required_str(remote, "remote")?;
            let mut options = with_remote_options_with_timeout(
                credential_callback,
                credential_user_data,
                mobile_timeout(timeout_ms),
            );
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

/// Publishes with an explicit native network timeout in milliseconds.
///
/// `timeout_ms == 0` disables Draftline's native network timeout for this call.
///
/// # Safety
///
/// `workspace` must be valid and `publish_token_json` must be valid
/// null-terminated UTF-8. Callback pointers must remain valid for this call.
#[no_mangle]
pub unsafe extern "C" fn draftline_mobile_workspace_publish_json_with_timeout(
    workspace: *mut DraftlineMobileWorkspace,
    publish_token_json: *const c_char,
    credential_callback: DraftlineMobileCredentialCallback,
    credential_user_data: *mut c_void,
    timeout_ms: u64,
) -> DraftlineMobileStringResult {
    match catch_unwind(AssertUnwindSafe(|| {
        string_result((|| {
            let handle = workspace_from_handle(workspace)?;
            let publish_token_json = required_str(publish_token_json, "publish_token_json")?;
            let token: PublishToken =
                serde_json::from_str(publish_token_json).map_err(FfiFailure::from)?;
            let mut options = with_remote_options_with_timeout(
                credential_callback,
                credential_user_data,
                mobile_timeout(timeout_ms),
            );
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
    use serde_json::Value;
    use std::{fs, path::Path, ptr};

    fn c_string(value: &str) -> CString {
        CString::new(value).unwrap()
    }

    fn write_file(root: &Path, relative: &str, content: &[u8]) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn read_file(root: &Path, relative: &str) -> String {
        fs::read_to_string(root.join(relative)).unwrap()
    }

    fn configure_identity(root: &Path) {
        let repo = git2::Repository::open(root).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Mobile Test").unwrap();
        config
            .set_str("user.email", "mobile@example.invalid")
            .unwrap();
    }

    fn init_bare_remote(root: &Path) {
        let mut options = git2::RepositoryInitOptions::new();
        options.bare(true).initial_head("main");
        git2::Repository::init_opts(root, &options).unwrap();
    }

    unsafe fn result_to_string(result: DraftlineMobileStringResult) -> String {
        assert_eq!(result.status.code, DraftlineMobileStatusCode::Ok);
        assert!(result.status.message.is_null());
        let value = CStr::from_ptr(result.value).to_str().unwrap().to_string();
        draftline_mobile_string_free(result.value);
        value
    }

    unsafe fn result_to_json(result: DraftlineMobileStringResult) -> Value {
        serde_json::from_str(&result_to_string(result)).unwrap()
    }

    #[test]
    fn mobile_timeout_zero_disables_explicit_timeout() {
        assert_eq!(mobile_timeout(0), None);
        assert_eq!(mobile_timeout(1), Some(Duration::from_millis(1)));
    }

    #[test]
    fn mobile_status_maps_timeout_errors() {
        let failure = FfiFailure::from(DraftlineError::RemoteOperationTimedOut {
            operation: "fetch_remote".to_string(),
            timeout_ms: 20_000,
        });

        assert_eq!(failure.code, DraftlineMobileStatusCode::RemoteTimeout);
        assert!(failure.message.contains("fetch_remote"));
    }

    #[test]
    fn mobile_status_maps_auth_and_network_errors() {
        let auth = FfiFailure::from(DraftlineError::Git(git2::Error::new(
            git2::ErrorCode::Auth,
            git2::ErrorClass::Net,
            "authentication failed",
        )));
        let network = FfiFailure::from(DraftlineError::Git(git2::Error::new(
            git2::ErrorCode::GenericError,
            git2::ErrorClass::Net,
            "network unavailable",
        )));

        assert_eq!(auth.code, DraftlineMobileStatusCode::CredentialRejected);
        assert_eq!(network.code, DraftlineMobileStatusCode::RemoteNetwork);
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

    #[test]
    fn mobile_bridge_shelves_previews_applies_and_deletes_json() {
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
        configure_identity(temp.path());

        write_file(temp.path(), "content/post.md", b"base");
        let base_label = c_string("Base");
        unsafe {
            result_to_string(draftline_mobile_workspace_save_version_json(
                opened.workspace,
                base_label.as_ptr(),
            ));
        }
        write_file(temp.path(), "content/post.md", b"edited");
        write_file(temp.path(), "content/extra.md", b"extra");

        let shelf_name = c_string("mobile-aside");
        let selected_paths = c_string(r#"["content/post.md"]"#);
        let selected_preflight = unsafe {
            result_to_json(draftline_mobile_workspace_preflight_shelve_json(
                opened.workspace,
                shelf_name.as_ptr(),
                selected_paths.as_ptr(),
            ))
        };
        assert_eq!(selected_preflight["operation"], "shelve_files:mobile-aside");
        assert_eq!(
            selected_preflight["dirty_files"].as_array().unwrap().len(),
            1
        );
        assert_eq!(selected_preflight["can_proceed"], true);

        let all_preflight = unsafe {
            result_to_json(draftline_mobile_workspace_preflight_shelve_json(
                opened.workspace,
                shelf_name.as_ptr(),
                ptr::null(),
            ))
        };
        assert_eq!(all_preflight["dirty_files"].as_array().unwrap().len(), 2);

        let shelf = unsafe {
            result_to_json(draftline_mobile_workspace_shelve_json(
                opened.workspace,
                shelf_name.as_ptr(),
                ptr::null(),
            ))
        };
        assert_eq!(shelf["id"], "mobile-aside");
        assert_eq!(read_file(temp.path(), "content/post.md"), "base");
        assert!(!temp.path().join("content/extra.md").exists());

        let shelves = unsafe {
            result_to_json(draftline_mobile_workspace_list_shelves_json(
                opened.workspace,
            ))
        };
        assert_eq!(shelves.as_array().unwrap().len(), 1);
        assert_eq!(shelves[0]["id"], "mobile-aside");

        let preview = unsafe {
            result_to_json(draftline_mobile_workspace_preview_shelf_json(
                opened.workspace,
                shelf_name.as_ptr(),
            ))
        };
        assert!(preview["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| { file["path"] == "content/post.md" && file["content"] == "edited" }));

        let apply_preflight = unsafe {
            result_to_json(draftline_mobile_workspace_preflight_apply_shelf_json(
                opened.workspace,
                shelf_name.as_ptr(),
            ))
        };
        assert_eq!(apply_preflight["can_proceed"], true);

        let applied = unsafe {
            result_to_json(draftline_mobile_workspace_apply_shelf_json(
                opened.workspace,
                shelf_name.as_ptr(),
            ))
        };
        assert_eq!(applied["id"], "mobile-aside");
        assert_eq!(read_file(temp.path(), "content/post.md"), "edited");
        assert_eq!(read_file(temp.path(), "content/extra.md"), "extra");

        let deleted = unsafe {
            result_to_json(draftline_mobile_workspace_delete_shelf_json(
                opened.workspace,
                shelf_name.as_ptr(),
            ))
        };
        assert_eq!(deleted["shelf_id"], "mobile-aside");
        assert_eq!(deleted["deleted"], true);
        let shelves = unsafe {
            result_to_json(draftline_mobile_workspace_list_shelves_json(
                opened.workspace,
            ))
        };
        assert!(shelves.as_array().unwrap().is_empty());

        unsafe { draftline_mobile_workspace_free(opened.workspace) };
    }

    #[test]
    fn mobile_bridge_preflights_and_resolves_merge_conflicts_json() {
        let remote_dir = tempfile::tempdir().unwrap();
        init_bare_remote(remote_dir.path());

        let author_dir = tempfile::tempdir().unwrap();
        let author = Workspace::init(author_dir.path()).unwrap();
        configure_identity(author.root());
        write_file(author.root(), "shared.md", b"base");
        author.save_version("Base").unwrap();
        author
            .add_remote("origin", remote_dir.path().to_string_lossy())
            .unwrap();
        author.publish_changes("origin").unwrap();

        let teammate_dir = tempfile::tempdir().unwrap();
        let teammate =
            Workspace::clone_workspace(remote_dir.path().to_string_lossy(), teammate_dir.path())
                .unwrap();
        configure_identity(teammate.root());

        write_file(author.root(), "shared.md", b"ours");
        author.save_version("Author local update").unwrap();
        write_file(teammate.root(), "shared.md", b"theirs");
        teammate.save_version("Teammate update").unwrap();
        teammate.publish_changes("origin").unwrap();

        let author_root = author.root().to_path_buf();
        let workspace = Box::into_raw(Box::new(DraftlineMobileWorkspace { workspace: author }));
        let remote = c_string("origin");
        let label = c_string("Resolved mobile merge");

        let fetch_status = unsafe {
            draftline_mobile_workspace_fetch_remote(
                workspace,
                remote.as_ptr(),
                None,
                ptr::null_mut(),
            )
        };
        assert_eq!(fetch_status.code, DraftlineMobileStatusCode::Ok);

        let preflight = unsafe {
            result_to_json(draftline_mobile_workspace_preflight_merge_incoming_json(
                workspace,
                remote.as_ptr(),
            ))
        };
        assert_eq!(preflight["sync_status"]["state"], "NeedsMerge");
        assert_eq!(preflight["conflicts"].as_array().unwrap().len(), 1);
        assert!(preflight["token"].is_object());

        let preflight_json = c_string(&preflight.to_string());
        let view_model = unsafe {
            result_to_json(draftline_mobile_merge_conflict_view_model_json(
                preflight_json.as_ptr(),
            ))
        };
        assert_eq!(view_model["files"].as_array().unwrap().len(), 1);
        assert_eq!(view_model["can_merge_cleanly"], false);

        let token_json = c_string(&preflight["token"].to_string());
        let resolutions_json = c_string(
            r#"[{"path":"shared.md","choice":{"kind":"use_content","content":"resolved"}}]"#,
        );
        let merged = unsafe {
            result_to_json(
                draftline_mobile_workspace_merge_incoming_with_resolutions_json(
                    workspace,
                    token_json.as_ptr(),
                    label.as_ptr(),
                    resolutions_json.as_ptr(),
                    None,
                    ptr::null_mut(),
                ),
            )
        };
        assert_eq!(merged["version"]["label"], "Resolved mobile merge");
        assert_eq!(merged["merged_files"][0], "shared.md");
        assert_eq!(read_file(&author_root, "shared.md"), "resolved");

        unsafe { draftline_mobile_workspace_free(workspace) };
    }
}

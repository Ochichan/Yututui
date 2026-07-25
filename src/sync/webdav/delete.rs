use super::*;

impl WebDavClient {
    pub async fn delete(
        &self,
        key: &ObjectKey,
        expected_etag: &str,
        credential: &VaultCredential,
    ) -> Result<ObjectDeleteResult, WebDavError> {
        self.delete_inner(key, expected_etag, credential, None)
            .await
    }

    pub(super) async fn delete_with_deadline(
        &self,
        key: &ObjectKey,
        expected_etag: &str,
        credential: &VaultCredential,
        deadline: VaultDeadline,
    ) -> Result<ObjectDeleteResult, WebDavError> {
        self.delete_inner(key, expected_etag, credential, Some(deadline))
            .await
    }

    async fn delete_inner(
        &self,
        key: &ObjectKey,
        expected_etag: &str,
        credential: &VaultCredential,
        deadline: Option<VaultDeadline>,
    ) -> Result<ObjectDeleteResult, WebDavError> {
        let response = match self
            .send_conditional_delete(key, expected_etag, credential, deadline)
            .await
        {
            Ok(response) => response,
            Err(WebDavError::RequestFailed) => {
                return self
                    .verify_ambiguous_delete(key, expected_etag, credential, deadline)
                    .await;
            }
            Err(error) => return Err(error),
        };
        match response.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(ObjectDeleteResult::Deleted),
            StatusCode::NOT_FOUND => Ok(ObjectDeleteResult::AlreadyAbsent),
            StatusCode::PRECONDITION_FAILED => Err(WebDavError::PreconditionFailed),
            StatusCode::ACCEPTED => {
                self.verify_ambiguous_delete(key, expected_etag, credential, deadline)
                    .await
            }
            status if status.is_server_error() => match status_error(&response) {
                error @ WebDavError::RateLimited(_) => Err(error),
                _ => {
                    self.verify_ambiguous_delete(key, expected_etag, credential, deadline)
                        .await
                }
            },
            _ => Err(status_error(&response)),
        }
    }

    async fn send_conditional_delete(
        &self,
        key: &ObjectKey,
        expected_etag: &str,
        credential: &VaultCredential,
        deadline: Option<VaultDeadline>,
    ) -> Result<Response, WebDavError> {
        let mut request =
            self.authenticated_request(Method::DELETE, self.object_url(key)?, credential)?;
        apply_match_condition(request.headers_mut(), expected_etag)?;
        self.execute_inner(request, deadline).await
    }

    async fn verify_ambiguous_delete(
        &self,
        key: &ObjectKey,
        expected_etag: &str,
        credential: &VaultCredential,
        deadline: Option<VaultDeadline>,
    ) -> Result<ObjectDeleteResult, WebDavError> {
        match self
            .get_inner(key, credential, MAX_PROTECTED_PAYLOAD_BYTES, deadline)
            .await
        {
            Ok(None) => Ok(ObjectDeleteResult::Deleted),
            Ok(Some((_, metadata))) if metadata.etag != expected_etag => {
                Err(WebDavError::PreconditionFailed)
            }
            Ok(Some(_)) => Err(WebDavError::AmbiguousWrite),
            Err(
                error @ (WebDavError::AuthenticationRequired
                | WebDavError::PermissionDenied
                | WebDavError::RequestFailed
                | WebDavError::MethodUnsupported
                | WebDavError::Locked
                | WebDavError::RateLimited(_)
                | WebDavError::ServerUnavailable),
            ) => Err(error),
            Err(_) => Err(WebDavError::AmbiguousWrite),
        }
    }
}

fn apply_match_condition(headers: &mut HeaderMap, raw: &str) -> Result<(), WebDavError> {
    let etag = EntityTag::parse(raw)?;
    etag.require_strong()?;
    let value = HeaderValue::from_str(etag.as_str()).map_err(|_| WebDavError::InvalidEntityTag)?;
    headers.insert(IF_MATCH, value);
    Ok(())
}

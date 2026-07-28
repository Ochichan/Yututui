//! Bounded OpenSubsonic capability discovery.

use std::collections::{BTreeMap, BTreeSet};

use super::client::{Endpoint, OpenSubsonicClient, ServerError, ServerInfo};
use super::private_store::ServerCredential;
use super::wire::RawExtension;

const MAX_EXTENSIONS: usize = 256;
const MAX_EXTENSION_NAME_CHARS: usize = 128;
const MAX_VERSIONS_PER_EXTENSION: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ServerFeature {
    Search,
    Albums,
    Artists,
    Songs,
    Playlists,
    CoverArt,
    Stream,
    FormPost,
    ApiKeyAuthentication,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerCapabilities {
    pub server_info: ServerInfo,
    extensions: BTreeMap<String, BTreeSet<u32>>,
}

impl ServerCapabilities {
    pub async fn probe(
        client: &OpenSubsonicClient,
        credential: &ServerCredential,
    ) -> Result<Self, ServerError> {
        // This endpoint is explicitly public in OpenSubsonic. A missing/broken implementation is
        // a feature downgrade, not a failed standard Subsonic connection.
        let extensions = match client.request_public_json(Endpoint::Extensions, &[]).await {
            Ok(response) => bounded_extensions(response.open_subsonic_extensions),
            Err(_) => BTreeMap::new(),
        };
        let server_info = client.ping(credential).await?;
        Ok(Self {
            server_info,
            extensions,
        })
    }

    pub fn supports(&self, feature: ServerFeature) -> bool {
        match feature {
            ServerFeature::Search
            | ServerFeature::Albums
            | ServerFeature::Artists
            | ServerFeature::Songs
            | ServerFeature::Playlists
            | ServerFeature::CoverArt
            | ServerFeature::Stream => true,
            ServerFeature::FormPost => self.has_extension("formPost", 1),
            ServerFeature::ApiKeyAuthentication => self.has_extension("apiKeyAuthentication", 1),
        }
    }

    pub fn extension_versions(&self, name: &str) -> Option<&BTreeSet<u32>> {
        self.extensions.get(name)
    }

    fn has_extension(&self, name: &str, version: u32) -> bool {
        self.extensions
            .get(name)
            .is_some_and(|versions| versions.contains(&version))
    }
}

fn bounded_extensions(raw: Vec<RawExtension>) -> BTreeMap<String, BTreeSet<u32>> {
    let mut extensions = BTreeMap::new();
    for extension in raw.into_iter().take(MAX_EXTENSIONS) {
        let Some(name) = extension.name.filter(|name| {
            !name.is_empty()
                && name.chars().count() <= MAX_EXTENSION_NAME_CHARS
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        }) else {
            continue;
        };
        let versions = extension
            .versions
            .into_iter()
            .take(MAX_VERSIONS_PER_EXTENSION)
            .collect::<BTreeSet<_>>();
        extensions
            .entry(name)
            .or_insert_with(BTreeSet::new)
            .extend(versions);
    }
    extensions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities(extensions: Vec<RawExtension>) -> ServerCapabilities {
        ServerCapabilities {
            server_info: ServerInfo {
                api_version: None,
                server_type: None,
                server_version: None,
                advertises_open_subsonic: true,
            },
            extensions: bounded_extensions(extensions),
        }
    }

    #[test]
    fn exact_extension_names_gate_optional_protocols() {
        let capabilities = capabilities(vec![
            RawExtension {
                name: Some("formPost".to_owned()),
                versions: vec![1],
            },
            RawExtension {
                name: Some("apiKeyAuthentication".to_owned()),
                versions: vec![1],
            },
        ]);
        assert!(capabilities.supports(ServerFeature::FormPost));
        assert!(capabilities.supports(ServerFeature::ApiKeyAuthentication));
        assert!(capabilities.supports(ServerFeature::Search));
    }

    #[test]
    fn extension_lists_are_bounded_and_sanitized() {
        let capabilities = capabilities(vec![
            RawExtension {
                name: Some("unsafe\u{202e}formPost".to_owned()),
                versions: vec![1],
            },
            RawExtension {
                name: Some("bounded".to_owned()),
                versions: (0..100).collect(),
            },
        ]);
        assert!(capabilities.extension_versions("unsafeformPost").is_none());
        assert_eq!(
            capabilities.extension_versions("bounded").unwrap().len(),
            MAX_VERSIONS_PER_EXTENSION
        );
    }
}

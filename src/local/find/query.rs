//! Structured Local Find query parser.

use std::fmt;

use unicode_normalization::UnicodeNormalization;

use super::LocalFindSort;
/// String field selected by a structured query prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LocalFindField {
    Any,
    Title,
    TrackArtist,
    Album,
    AlbumArtist,
    Genre,
    Path,
    Format,
}

/// A normalized substring term. Terms are ANDed together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFindTerm {
    pub field: LocalFindField,
    pub value: String,
}

/// Inclusive year constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalFindYearRange {
    pub start: i32,
    pub end: i32,
}

impl LocalFindYearRange {
    pub fn contains(self, year: i32) -> bool {
        (self.start..=self.end).contains(&year)
    }
}

/// Boolean local-media predicates supported by `is:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LocalFindIs {
    Lossless,
    LocalOnly,
    Downloaded,
}

/// Missing-metadata predicates supported by `missing:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LocalFindMissing {
    Artist,
    Album,
    Cover,
}

/// Closed command set. No parsed value can represent a shell or arbitrary command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LocalFindCommand {
    Rescan,
    Rebuild,
    Queue,
    Tracks,
    Albums,
    Artists,
    Genres,
    Folders,
    Playlists,
    ScanErrors,
}

impl LocalFindCommand {
    pub const ALL: [Self; 10] = [
        Self::Rescan,
        Self::Rebuild,
        Self::Queue,
        Self::Tracks,
        Self::Albums,
        Self::Artists,
        Self::Genres,
        Self::Folders,
        Self::Playlists,
        Self::ScanErrors,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rescan => "rescan",
            Self::Rebuild => "rebuild",
            Self::Queue => "queue",
            Self::Tracks => "tracks",
            Self::Albums => "albums",
            Self::Artists => "artists",
            Self::Genres => "genres",
            Self::Folders => "folders",
            Self::Playlists => "playlists",
            Self::ScanErrors => "scan errors",
        }
    }
}

/// Safe parse result for a `>` command query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFindCommandQuery {
    pub input: String,
    pub exact: Option<LocalFindCommand>,
    pub suggestions: Vec<LocalFindCommand>,
}

impl LocalFindCommandQuery {
    pub fn parse(input: &str) -> Self {
        let input = normalize_command(input);
        let exact = LocalFindCommand::ALL
            .into_iter()
            .find(|command| command.as_str() == input);
        let suggestions = if let Some(exact) = exact {
            vec![exact]
        } else {
            LocalFindCommand::ALL
                .into_iter()
                .filter(|command| input.is_empty() || command.as_str().starts_with(&input))
                .collect()
        };
        Self {
            input,
            exact,
            suggestions,
        }
    }
}

/// Parsed Local Find query. All string values are NFKC-normalized and lowercased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFindQuery {
    pub raw: String,
    pub terms: Vec<LocalFindTerm>,
    pub years: Vec<LocalFindYearRange>,
    pub predicates: Vec<LocalFindIs>,
    pub missing: Vec<LocalFindMissing>,
    pub sort_override: Option<LocalFindSort>,
    pub command: Option<LocalFindCommandQuery>,
}

impl LocalFindQuery {
    pub fn parse(raw: &str) -> Result<Self, LocalFindParseError> {
        let trimmed = raw.trim();
        if let Some(command) = trimmed.strip_prefix('>') {
            return Ok(Self {
                raw: raw.to_owned(),
                terms: Vec::new(),
                years: Vec::new(),
                predicates: Vec::new(),
                missing: Vec::new(),
                sort_override: None,
                command: Some(LocalFindCommandQuery::parse(command)),
            });
        }

        let mut query = Self {
            raw: raw.to_owned(),
            terms: Vec::new(),
            years: Vec::new(),
            predicates: Vec::new(),
            missing: Vec::new(),
            sort_override: None,
            command: None,
        };

        for token in tokenize_query(trimmed)? {
            let normalized = normalize_text(&token);
            let normalized = normalized.trim();
            let Some((prefix, value)) = normalized.split_once(':') else {
                if !normalized.is_empty() {
                    query.terms.push(LocalFindTerm {
                        field: LocalFindField::Any,
                        value: normalized.to_owned(),
                    });
                }
                continue;
            };

            let field = match prefix {
                "t" => Some(LocalFindField::Title),
                "ar" => Some(LocalFindField::TrackArtist),
                "al" => Some(LocalFindField::Album),
                "aa" => Some(LocalFindField::AlbumArtist),
                "g" => Some(LocalFindField::Genre),
                "path" => Some(LocalFindField::Path),
                "fmt" => Some(LocalFindField::Format),
                _ => None,
            };
            if let Some(field) = field {
                require_value(prefix, value, &token)?;
                query.terms.push(LocalFindTerm {
                    field,
                    value: value.to_owned(),
                });
                continue;
            }

            match prefix {
                "year" => {
                    require_value(prefix, value, &token)?;
                    query.years.push(parse_year(value, &token)?);
                }
                "is" => {
                    require_value(prefix, value, &token)?;
                    let predicate = match value {
                        "lossless" => LocalFindIs::Lossless,
                        "local-only" => LocalFindIs::LocalOnly,
                        "downloaded" => LocalFindIs::Downloaded,
                        _ => {
                            return Err(LocalFindParseError::invalid_value(
                                &token,
                                "is",
                                "lossless, local-only, or downloaded",
                            ));
                        }
                    };
                    query.predicates.push(predicate);
                }
                "missing" => {
                    require_value(prefix, value, &token)?;
                    let missing = match value {
                        "artist" => LocalFindMissing::Artist,
                        "album" => LocalFindMissing::Album,
                        "cover" => LocalFindMissing::Cover,
                        _ => {
                            return Err(LocalFindParseError::invalid_value(
                                &token,
                                "missing",
                                "artist, album, or cover",
                            ));
                        }
                    };
                    query.missing.push(missing);
                }
                "sort" => {
                    require_value(prefix, value, &token)?;
                    query.sort_override = Some(LocalFindSort::parse(value).ok_or_else(|| {
                        LocalFindParseError::invalid_value(
                            &token,
                            "sort",
                            "relevance, title, artist, album, year, or recent",
                        )
                    })?);
                }
                // An unknown prefix is deliberately ordinary literal text.
                _ => query.terms.push(LocalFindTerm {
                    field: LocalFindField::Any,
                    value: normalized.to_owned(),
                }),
            }
        }

        Ok(query)
    }

    pub fn effective_sort(&self, base: LocalFindSort) -> LocalFindSort {
        self.sort_override.unwrap_or(base)
    }

    /// True only for a genuinely blank text query. `sort:recent` intentionally searches all.
    pub fn is_blank(&self) -> bool {
        self.command.is_none()
            && self.terms.is_empty()
            && self.years.is_empty()
            && self.predicates.is_empty()
            && self.missing.is_empty()
            && self.sort_override.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalFindParseErrorKind {
    UnclosedQuote,
    EmptyValue,
    InvalidYear,
    InvalidValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFindParseError {
    pub kind: LocalFindParseErrorKind,
    pub token: String,
    message: String,
}

impl LocalFindParseError {
    fn new(kind: LocalFindParseErrorKind, token: &str, message: impl Into<String>) -> Self {
        Self {
            kind,
            token: token.to_owned(),
            message: message.into(),
        }
    }

    fn invalid_value(token: &str, prefix: &str, expected: &str) -> Self {
        Self::new(
            LocalFindParseErrorKind::InvalidValue,
            token,
            format!("{prefix}: expects {expected}"),
        )
    }

    /// User-facing parse feedback in the application's supported UI languages. Keep the typed
    /// error kind in the parser so the reducer never has to translate brittle English strings.
    pub fn localized_message(&self) -> String {
        if !crate::i18n::is_korean() {
            return self.message.clone();
        }
        match self.kind {
            LocalFindParseErrorKind::UnclosedQuote => "닫히지 않은 따옴표가 있습니다".to_owned(),
            LocalFindParseErrorKind::EmptyValue => {
                format!("'{}'에 값이 필요합니다", self.token)
            }
            LocalFindParseErrorKind::InvalidYear => {
                format!("유효하지 않은 연도 범위입니다: '{}'", self.token)
            }
            LocalFindParseErrorKind::InvalidValue => {
                format!("지원하지 않는 검색 값입니다: '{}'", self.token)
            }
        }
    }
}

impl fmt::Display for LocalFindParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LocalFindParseError {}

fn require_value(prefix: &str, value: &str, token: &str) -> Result<(), LocalFindParseError> {
    if value.trim().is_empty() {
        return Err(LocalFindParseError::new(
            LocalFindParseErrorKind::EmptyValue,
            token,
            format!("{prefix}: requires a value"),
        ));
    }
    Ok(())
}

fn parse_year(value: &str, token: &str) -> Result<LocalFindYearRange, LocalFindParseError> {
    let (start, end) = match value.split_once("..") {
        Some((start, end)) => (
            parse_year_value(start, token)?,
            parse_year_value(end, token)?,
        ),
        None => {
            let year = parse_year_value(value, token)?;
            (year, year)
        }
    };
    if start > end {
        return Err(LocalFindParseError::new(
            LocalFindParseErrorKind::InvalidYear,
            token,
            "year: range start must not exceed its end",
        ));
    }
    Ok(LocalFindYearRange { start, end })
}

fn parse_year_value(value: &str, token: &str) -> Result<i32, LocalFindParseError> {
    let year = value.parse::<i32>().map_err(|_| {
        LocalFindParseError::new(
            LocalFindParseErrorKind::InvalidYear,
            token,
            "year: expects a year or inclusive start..end range",
        )
    })?;
    if !(0..=9999).contains(&year) {
        return Err(LocalFindParseError::new(
            LocalFindParseErrorKind::InvalidYear,
            token,
            "year: must be between 0 and 9999",
        ));
    }
    Ok(year)
}

fn tokenize_query(input: &str) -> Result<Vec<String>, LocalFindParseError> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut saw_quote = false;
    for ch in input.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                saw_quote = true;
            }
            ch if ch.is_whitespace() && !quoted => {
                if !token.is_empty() || saw_quote {
                    tokens.push(std::mem::take(&mut token));
                    saw_quote = false;
                }
            }
            _ => token.push(ch),
        }
    }
    if quoted {
        return Err(LocalFindParseError::new(
            LocalFindParseErrorKind::UnclosedQuote,
            input,
            "query contains an unclosed quote",
        ));
    }
    if !token.is_empty() || saw_quote {
        tokens.push(token);
    }
    Ok(tokens)
}

pub(super) fn normalize_text(text: &str) -> String {
    text.nfkc().flat_map(char::to_lowercase).collect()
}

fn normalize_command(text: &str) -> String {
    normalize_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

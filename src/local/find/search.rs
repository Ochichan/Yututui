//! Query evaluation and ranking for the immutable Local Find corpus.

use super::*;

impl LocalFindCorpus {
    pub fn search(
        &self,
        query: &LocalFindQuery,
        scope: LocalFindScope,
        base_sort: LocalFindSort,
        generation: u64,
    ) -> LocalFindSnapshot {
        self.search_cancellable(query, scope, base_sort, generation, || false)
            .expect("a search with cancellation disabled always completes")
    }

    /// Search while cooperatively polling for obsolescence.
    ///
    /// Runtime workers use this to retire superseded per-keystroke evaluations. Polling is kept
    /// inside the pure in-memory loop; no filesystem or network work is introduced here.
    pub fn search_cancellable(
        &self,
        query: &LocalFindQuery,
        scope: LocalFindScope,
        base_sort: LocalFindSort,
        generation: u64,
        should_cancel: impl Fn() -> bool,
    ) -> Option<LocalFindSnapshot> {
        if should_cancel() {
            return None;
        }
        let sort = query.effective_sort(base_sort);
        let groups = if let Some(command) = &query.command {
            let commands = command
                .suggestions
                .iter()
                .copied()
                .map(|command| LocalFindHit {
                    id: LocalFindHitId::Command(command),
                    label: format!("> {}", command.as_str()),
                    secondary: String::new(),
                    year: None,
                    locally_playable_count: 0,
                    total_track_count: 0,
                    match_reason: None,
                })
                .collect::<Vec<_>>();
            if commands.is_empty() {
                Vec::new()
            } else {
                vec![LocalFindGroup {
                    scope: LocalFindScope::All,
                    hits: commands,
                }]
            }
        } else if query.is_blank() {
            Vec::new()
        } else {
            self.search_groups(query, scope, sort, &should_cancel)?
        };
        if should_cancel() {
            return None;
        }
        let total_hits = groups.iter().map(|group| group.hits.len()).sum();
        Some(LocalFindSnapshot {
            generation,
            corpus_revision: self.revision,
            query: query.clone(),
            scope,
            sort,
            groups,
            total_hits,
        })
    }

    fn search_groups(
        &self,
        query: &LocalFindQuery,
        scope: LocalFindScope,
        sort: LocalFindSort,
        should_cancel: &impl Fn() -> bool,
    ) -> Option<Vec<LocalFindGroup>> {
        let scopes: &[LocalFindScope] = if scope == LocalFindScope::All {
            &LocalFindScope::RESULT_GROUPS
        } else {
            std::slice::from_ref(&scope)
        };
        let mut groups = Vec::new();
        for result_scope in scopes {
            if should_cancel() {
                return None;
            }
            let mut matches = Vec::new();
            for (index, document) in self.documents(*result_scope).iter().enumerate() {
                if index.is_multiple_of(64) && should_cancel() {
                    return None;
                }
                if *result_scope == LocalFindScope::Playlists
                    && scope != LocalFindScope::Playlists
                    && document.locally_playable_count == 0
                {
                    continue;
                }
                let match_reason =
                    match self.document_match(document, query, *result_scope, should_cancel) {
                        DocumentMatch::Cancelled => return None,
                        DocumentMatch::No => continue,
                        DocumentMatch::Yes(reason) => reason,
                    };
                let relevance = self.relevance(document, query, should_cancel)?;
                matches.push(MatchedDocument {
                    document,
                    relevance,
                    match_reason,
                });
            }
            if !matches.is_empty() {
                matches.sort_by(|left, right| compare_matches(left, right, sort));
                groups.push(LocalFindGroup {
                    scope: *result_scope,
                    hits: matches
                        .into_iter()
                        .map(|matched| {
                            let mut hit = matched.document.hit();
                            hit.match_reason = matched.match_reason;
                            hit
                        })
                        .collect(),
                });
            }
        }
        Some(groups)
    }

    fn documents(&self, scope: LocalFindScope) -> &[SearchDocument] {
        match scope {
            LocalFindScope::All | LocalFindScope::Tracks => &self.tracks,
            LocalFindScope::Albums => &self.albums,
            LocalFindScope::Artists => &self.artists,
            LocalFindScope::Genres => &self.genres,
            LocalFindScope::Folders => &self.folders,
            LocalFindScope::Playlists => &self.playlists,
        }
    }

    fn document_match(
        &self,
        document: &SearchDocument,
        query: &LocalFindQuery,
        scope: LocalFindScope,
        should_cancel: &impl Fn() -> bool,
    ) -> DocumentMatch {
        if scope == LocalFindScope::Playlists {
            return self.playlist_match(document, query, should_cancel);
        }
        match self.document_matches(document, query, should_cancel) {
            Some(true) => DocumentMatch::Yes(None),
            Some(false) => DocumentMatch::No,
            None => DocumentMatch::Cancelled,
        }
    }

    fn document_matches(
        &self,
        document: &SearchDocument,
        query: &LocalFindQuery,
        should_cancel: &impl Fn() -> bool,
    ) -> Option<bool> {
        for term in &query.terms {
            if !document_term_matches(document, term)
                && !self.any_track(&document.track_ids, should_cancel, |track| {
                    track_term_matches(track, term)
                })?
            {
                return Some(false);
            }
        }
        for range in &query.years {
            if !document.year.is_some_and(|year| range.contains(year))
                && !self.any_track(&document.track_ids, should_cancel, |track| {
                    track.year.is_some_and(|year| range.contains(year))
                })?
            {
                return Some(false);
            }
        }
        for predicate in &query.predicates {
            if !self.any_track(&document.track_ids, should_cancel, |track| {
                track.predicates.contains(predicate)
            })? {
                return Some(false);
            }
        }
        for missing in &query.missing {
            if !self.any_track(&document.track_ids, should_cancel, |track| {
                track.missing.contains(missing)
            })? {
                return Some(false);
            }
        }
        Some(true)
    }

    /// A playlist is one logical container, but its filters must never be satisfied by mixing
    /// metadata from different entries. Either its name satisfies the whole name-only query, or
    /// one resolved local track satisfies every term/predicate in the query.
    fn playlist_match(
        &self,
        document: &SearchDocument,
        query: &LocalFindQuery,
        should_cancel: &impl Fn() -> bool,
    ) -> DocumentMatch {
        let has_filter = !query.terms.is_empty()
            || !query.years.is_empty()
            || !query.predicates.is_empty()
            || !query.missing.is_empty();
        if !has_filter {
            return DocumentMatch::Yes(None);
        }
        let name_matches = query.years.is_empty()
            && query.predicates.is_empty()
            && query.missing.is_empty()
            && query
                .terms
                .iter()
                .all(|term| document_term_matches(document, term));
        if name_matches {
            return DocumentMatch::Yes(Some(LocalFindMatchReason::PlaylistName));
        }
        match self.any_track(&document.track_ids, should_cancel, |track| {
            track_matches_query(track, query)
        }) {
            Some(true) => DocumentMatch::Yes(Some(LocalFindMatchReason::ResolvedLocalTrack)),
            Some(false) => DocumentMatch::No,
            None => DocumentMatch::Cancelled,
        }
    }

    fn any_track(
        &self,
        ids: &[LocalTrackId],
        should_cancel: &impl Fn() -> bool,
        mut predicate: impl FnMut(&TrackSearchData) -> bool,
    ) -> Option<bool> {
        for (index, id) in ids.iter().enumerate() {
            if index.is_multiple_of(64) && should_cancel() {
                return None;
            }
            if let Some(track) = self.track_data.get(id)
                && predicate(track)
            {
                return Some(true);
            }
        }
        Some(false)
    }

    fn relevance(
        &self,
        document: &SearchDocument,
        query: &LocalFindQuery,
        should_cancel: &impl Fn() -> bool,
    ) -> Option<u8> {
        let title_terms = query
            .terms
            .iter()
            .filter(|term| term.field == LocalFindField::Title)
            .map(|term| term.value.as_str())
            .collect::<Vec<_>>();
        if !title_terms.is_empty() {
            return self.title_relevance(document, &title_terms, should_cancel);
        }

        let terms = query
            .terms
            .iter()
            .filter(|term| term.field == LocalFindField::Any)
            .map(|term| term.value.as_str())
            .collect::<Vec<_>>();
        if terms.is_empty() {
            return Some(3);
        }
        let phrase = terms.join(" ");
        if document.label_sort == phrase {
            return Some(0);
        }
        if document.label_sort.starts_with(&phrase) {
            return Some(1);
        }
        let mut all_prefix = true;
        for term in &terms {
            if !self.major_term_matches(document, term, true, should_cancel)? {
                all_prefix = false;
                break;
            }
        }
        if all_prefix {
            return Some(2);
        }
        for term in &terms {
            if !self.major_term_matches(document, term, false, should_cancel)? {
                return Some(4);
            }
        }
        Some(3)
    }

    fn title_relevance(
        &self,
        document: &SearchDocument,
        terms: &[&str],
        should_cancel: &impl Fn() -> bool,
    ) -> Option<u8> {
        let phrase = terms.join(" ");
        let mut best = None;
        for (index, id) in document.track_ids.iter().enumerate() {
            if index.is_multiple_of(64) && should_cancel() {
                return None;
            }
            let Some(track) = self.track_data.get(id) else {
                continue;
            };
            for title in track.fields.values(LocalFindField::Title) {
                best = min_rank(best, terms_relevance(title, terms, &phrase));
            }
            if best == Some(0) {
                break;
            }
        }
        // Matching already enforced AND semantics. A collection whose title constraints were
        // distributed across entries is deliberately ranked behind a same-title match.
        Some(best.unwrap_or(3))
    }

    fn major_term_matches(
        &self,
        document: &SearchDocument,
        term: &str,
        prefix: bool,
        should_cancel: &impl Fn() -> bool,
    ) -> Option<bool> {
        let matches = |value: &str| {
            if prefix {
                word_prefix_match(value, term)
            } else {
                value.contains(term)
            }
        };
        if matches(&document.label_sort)
            || matches(&document.secondary_sort)
            || document.fields.major_values().any(matches)
        {
            return Some(true);
        }
        self.any_track(&document.track_ids, should_cancel, |track| {
            track.fields.major_values().any(matches)
        })
    }
}

fn min_rank(left: Option<u8>, right: Option<u8>) -> Option<u8> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(rank), None) | (None, Some(rank)) => Some(rank),
        (None, None) => None,
    }
}

fn terms_relevance(value: &str, terms: &[&str], phrase: &str) -> Option<u8> {
    if value == phrase {
        Some(0)
    } else if terms.iter().all(|term| word_prefix_match(value, term)) {
        Some(1)
    } else if terms.iter().all(|term| value.contains(term)) {
        Some(2)
    } else {
        None
    }
}

fn track_term_matches(track: &TrackSearchData, term: &LocalFindTerm) -> bool {
    match term.field {
        LocalFindField::Any => track
            .fields
            .all_values()
            .any(|value| value.contains(&term.value)),
        LocalFindField::Format => track
            .fields
            .values(LocalFindField::Format)
            .any(|value| value == term.value),
        field => track
            .fields
            .values(field)
            .any(|value| value.contains(&term.value)),
    }
}

fn document_term_matches(document: &SearchDocument, term: &LocalFindTerm) -> bool {
    match term.field {
        LocalFindField::Any => {
            document.label_sort.contains(&term.value)
                || document.secondary_sort.contains(&term.value)
                || document
                    .fields
                    .all_values()
                    .any(|value| value.contains(&term.value))
        }
        LocalFindField::Format => document
            .fields
            .values(LocalFindField::Format)
            .any(|value| value == term.value),
        field => document
            .fields
            .values(field)
            .any(|value| value.contains(&term.value)),
    }
}

fn track_matches_query(track: &TrackSearchData, query: &LocalFindQuery) -> bool {
    query
        .terms
        .iter()
        .all(|term| track_term_matches(track, term))
        && query
            .years
            .iter()
            .all(|range| track.year.is_some_and(|year| range.contains(year)))
        && query
            .predicates
            .iter()
            .all(|predicate| track.predicates.contains(predicate))
        && query
            .missing
            .iter()
            .all(|missing| track.missing.contains(missing))
}

fn word_prefix_match(value: &str, term: &str) -> bool {
    value
        .split(|ch: char| !ch.is_alphanumeric())
        .any(|word| word.starts_with(term))
}

struct MatchedDocument<'a> {
    document: &'a SearchDocument,
    relevance: u8,
    match_reason: Option<LocalFindMatchReason>,
}

enum DocumentMatch {
    No,
    Yes(Option<LocalFindMatchReason>),
    Cancelled,
}

fn compare_matches(
    left: &MatchedDocument<'_>,
    right: &MatchedDocument<'_>,
    sort: LocalFindSort,
) -> Ordering {
    let left_doc = left.document;
    let right_doc = right.document;
    let primary = match sort {
        LocalFindSort::Relevance => left.relevance.cmp(&right.relevance),
        LocalFindSort::Title => Ordering::Equal,
        LocalFindSort::Artist => empty_last(&left_doc.artist_sort, &right_doc.artist_sort),
        LocalFindSort::Album => empty_last(&left_doc.album_sort, &right_doc.album_sort),
        LocalFindSort::Year => option_desc(left_doc.year, right_doc.year),
        LocalFindSort::Recent => right_doc.recent.cmp(&left_doc.recent),
    };
    primary
        .then_with(|| left_doc.label_sort.cmp(&right_doc.label_sort))
        .then_with(|| left_doc.secondary_sort.cmp(&right_doc.secondary_sort))
        .then_with(|| left_doc.id.cmp(&right_doc.id))
}

fn empty_last(left: &str, right: &str) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (false, true) => Ordering::Less,
        (true, false) => Ordering::Greater,
        _ => left.cmp(right),
    }
}

fn option_desc<T: Ord>(left: Option<T>, right: Option<T>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

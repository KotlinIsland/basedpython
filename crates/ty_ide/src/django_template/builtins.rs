//! django's builtin template tags and filters
//!
//! these are the ones `django.template.defaulttags`, `django.template.defaultfilters`
//! and the libraries shipped in `django.templatetags` register. a project's own
//! tags and filters are discovered from its source instead — see [`super::project`].
//!
//! the tables carry the block structure (which tag closes which, and which tags
//! may appear in between) because the completions and the index both need it: a
//! `{% for %}` without the knowledge that `{% empty %}` belongs inside it would
//! either close the block early or never offer `{% empty %}` at all.

/// a builtin template tag
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tag {
    pub(crate) name: &'static str,
    /// the tag closing the block this one opens, for a block tag
    pub(crate) closed_by: Option<&'static str>,
    /// the tags that may appear between this tag and the one that closes it
    pub(crate) branches: &'static [&'static str],
    /// the `{% load %}` library providing this tag, or `None` when it is always
    /// available
    pub(crate) library: Option<&'static str>,
    pub(crate) documentation: &'static str,
}

/// a builtin template filter
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Filter {
    pub(crate) name: &'static str,
    /// the `{% load %}` library providing this filter, or `None` when it is
    /// always available
    pub(crate) library: Option<&'static str>,
    pub(crate) documentation: &'static str,
}

/// look a builtin tag up by name
pub(crate) fn tag(name: &str) -> Option<&'static Tag> {
    TAGS.iter().find(|tag| tag.name == name)
}

/// look a builtin filter up by name
pub(crate) fn filter(name: &str) -> Option<&'static Filter> {
    FILTERS.iter().find(|filter| filter.name == name)
}

/// the tag closing the block `name` opens, whether `name` is builtin or one of
/// the project's own block tags
pub(crate) fn end_tag_for(name: &str) -> Option<&'static str> {
    tag(name).and_then(|tag| tag.closed_by)
}

/// the libraries a `{% load %}` can name
pub(crate) const LIBRARIES: &[&str] = &["cache", "i18n", "l10n", "static", "tz"];

pub(crate) const TAGS: &[Tag] = &[
    Tag {
        name: "autoescape",
        closed_by: Some("endautoescape"),
        branches: &[],
        library: None,
        documentation: "controls html auto-escaping for the enclosed block. takes `on` or `off`.",
    },
    Tag {
        name: "block",
        closed_by: Some("endblock"),
        branches: &[],
        library: None,
        documentation: "defines a named block a child template can override.",
    },
    Tag {
        name: "comment",
        closed_by: Some("endcomment"),
        branches: &[],
        library: None,
        documentation: "ignores everything between the tags.",
    },
    Tag {
        name: "csrf_token",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "renders the hidden csrf token input. required in every `POST` form.",
    },
    Tag {
        name: "cycle",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "emits the next of its arguments each time it is reached.",
    },
    Tag {
        name: "debug",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "outputs the whole current context, for debugging.",
    },
    Tag {
        name: "extends",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "declares this template a child of another. must be the first tag in the file.",
    },
    Tag {
        name: "filter",
        closed_by: Some("endfilter"),
        branches: &[],
        library: None,
        documentation: "runs the enclosed block's output through the given filters.",
    },
    Tag {
        name: "firstof",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "outputs the first of its arguments that is truthy.",
    },
    Tag {
        name: "for",
        closed_by: Some("endfor"),
        branches: &["empty"],
        library: None,
        documentation: "loops over each item of a sequence. `{% empty %}` supplies the body for an empty one.",
    },
    Tag {
        name: "if",
        closed_by: Some("endif"),
        branches: &["elif", "else"],
        library: None,
        documentation: "renders its body when the condition is truthy.",
    },
    Tag {
        name: "ifchanged",
        closed_by: Some("endifchanged"),
        branches: &["else"],
        library: None,
        documentation: "renders its body only when the value has changed since the last loop iteration.",
    },
    Tag {
        name: "include",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "renders another template here, with the current context or a `with` one.",
    },
    Tag {
        name: "load",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "loads a template tag library, making its tags and filters available.",
    },
    Tag {
        name: "lorem",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "emits placeholder lorem ipsum text.",
    },
    Tag {
        name: "now",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "formats the current date and time with the given format string.",
    },
    Tag {
        name: "partial",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "renders a fragment defined by a `{% partialdef %}`.",
    },
    Tag {
        name: "partialdef",
        closed_by: Some("endpartialdef"),
        branches: &[],
        library: None,
        documentation: "defines a named, reusable fragment of this template. `inline` also renders it in place.",
    },
    Tag {
        name: "querystring",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "renders a url-encoded query string from the request's, with the given changes applied.",
    },
    Tag {
        name: "regroup",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "regroups a list of objects by a common attribute.",
    },
    Tag {
        name: "resetcycle",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "restarts a `{% cycle %}` from its first argument.",
    },
    Tag {
        name: "spaceless",
        closed_by: Some("endspaceless"),
        branches: &[],
        library: None,
        documentation: "strips the whitespace between html tags in its body.",
    },
    Tag {
        name: "templatetag",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "outputs one of the template language's own delimiters, such as `openblock`.",
    },
    Tag {
        name: "url",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "reverses a named url pattern into its path.",
    },
    Tag {
        name: "verbatim",
        closed_by: Some("endverbatim"),
        branches: &[],
        library: None,
        documentation: "outputs its body without rendering any template syntax in it.",
    },
    Tag {
        name: "widthratio",
        closed_by: None,
        branches: &[],
        library: None,
        documentation: "scales a value against a maximum, for bar-chart widths.",
    },
    Tag {
        name: "with",
        closed_by: Some("endwith"),
        branches: &[],
        library: None,
        documentation: "binds names to values for the enclosed block.",
    },
    // `cache`
    Tag {
        name: "cache",
        closed_by: Some("endcache"),
        branches: &[],
        library: Some("cache"),
        documentation: "caches the rendered body for the given number of seconds, keyed by the given name.",
    },
    // `i18n`
    Tag {
        name: "blocktranslate",
        closed_by: Some("endblocktranslate"),
        branches: &["plural"],
        library: Some("i18n"),
        documentation: "marks a block of text for translation, with placeholders for variables.",
    },
    Tag {
        name: "blocktrans",
        closed_by: Some("endblocktrans"),
        branches: &["plural"],
        library: Some("i18n"),
        documentation: "the older spelling of `{% blocktranslate %}`.",
    },
    Tag {
        name: "get_available_languages",
        closed_by: None,
        branches: &[],
        library: Some("i18n"),
        documentation: "binds the list of configured `(code, name)` language pairs to a variable.",
    },
    Tag {
        name: "get_current_language",
        closed_by: None,
        branches: &[],
        library: Some("i18n"),
        documentation: "binds the active language's code to a variable.",
    },
    Tag {
        name: "get_current_language_bidi",
        closed_by: None,
        branches: &[],
        library: Some("i18n"),
        documentation: "binds whether the active language is right-to-left to a variable.",
    },
    Tag {
        name: "get_language_info",
        closed_by: None,
        branches: &[],
        library: Some("i18n"),
        documentation: "binds a language's name, code and direction to a variable.",
    },
    Tag {
        name: "get_language_info_list",
        closed_by: None,
        branches: &[],
        library: Some("i18n"),
        documentation: "binds the language info of each of the given codes to a variable.",
    },
    Tag {
        name: "language",
        closed_by: Some("endlanguage"),
        branches: &[],
        library: Some("i18n"),
        documentation: "renders its body with the given language active.",
    },
    Tag {
        name: "translate",
        closed_by: None,
        branches: &[],
        library: Some("i18n"),
        documentation: "translates a string literal or variable.",
    },
    Tag {
        name: "trans",
        closed_by: None,
        branches: &[],
        library: Some("i18n"),
        documentation: "the older spelling of `{% translate %}`.",
    },
    // `l10n`
    Tag {
        name: "localize",
        closed_by: Some("endlocalize"),
        branches: &[],
        library: Some("l10n"),
        documentation: "turns locale-aware number formatting on or off for its body.",
    },
    // `static`
    Tag {
        name: "get_media_prefix",
        closed_by: None,
        branches: &[],
        library: Some("static"),
        documentation: "binds `MEDIA_URL` to a variable.",
    },
    Tag {
        name: "get_static_prefix",
        closed_by: None,
        branches: &[],
        library: Some("static"),
        documentation: "binds `STATIC_URL` to a variable.",
    },
    Tag {
        name: "static",
        closed_by: None,
        branches: &[],
        library: Some("static"),
        documentation: "builds the url of a static file.",
    },
    // `tz`
    Tag {
        name: "get_current_timezone",
        closed_by: None,
        branches: &[],
        library: Some("tz"),
        documentation: "binds the active time zone's name to a variable.",
    },
    Tag {
        name: "localtime",
        closed_by: Some("endlocaltime"),
        branches: &[],
        library: Some("tz"),
        documentation: "turns conversion of datetimes to local time on or off for its body.",
    },
    Tag {
        name: "timezone",
        closed_by: Some("endtimezone"),
        branches: &[],
        library: Some("tz"),
        documentation: "renders its body with the given time zone active.",
    },
];

pub(crate) const FILTERS: &[Filter] = &[
    Filter {
        name: "add",
        library: None,
        documentation: "adds the argument to the value.",
    },
    Filter {
        name: "addslashes",
        library: None,
        documentation: "backslash-escapes quotes.",
    },
    Filter {
        name: "capfirst",
        library: None,
        documentation: "upper-cases the first character.",
    },
    Filter {
        name: "center",
        library: None,
        documentation: "centres the value in a field of the given width.",
    },
    Filter {
        name: "cut",
        library: None,
        documentation: "removes every occurrence of the argument.",
    },
    Filter {
        name: "date",
        library: None,
        documentation: "formats a date with the given format string.",
    },
    Filter {
        name: "default",
        library: None,
        documentation: "uses the argument when the value is falsy.",
    },
    Filter {
        name: "default_if_none",
        library: None,
        documentation: "uses the argument only when the value is `None`.",
    },
    Filter {
        name: "dictsort",
        library: None,
        documentation: "sorts a list of mappings by the given key.",
    },
    Filter {
        name: "dictsortreversed",
        library: None,
        documentation: "sorts a list of mappings by the given key, descending.",
    },
    Filter {
        name: "divisibleby",
        library: None,
        documentation: "whether the value divides by the argument.",
    },
    Filter {
        name: "escape",
        library: None,
        documentation: "html-escapes the value.",
    },
    Filter {
        name: "escapejs",
        library: None,
        documentation: "escapes the value for use in a javascript string.",
    },
    Filter {
        name: "escapeseq",
        library: None,
        documentation: "html-escapes each element of a sequence.",
    },
    Filter {
        name: "filesizeformat",
        library: None,
        documentation: "formats a byte count as `13 KB`.",
    },
    Filter {
        name: "first",
        library: None,
        documentation: "the first element.",
    },
    Filter {
        name: "floatformat",
        library: None,
        documentation: "rounds a float to the given number of decimal places.",
    },
    Filter {
        name: "force_escape",
        library: None,
        documentation: "html-escapes the value immediately rather than lazily.",
    },
    Filter {
        name: "get_digit",
        library: None,
        documentation: "the nth digit of an integer, counted from the right.",
    },
    Filter {
        name: "iriencode",
        library: None,
        documentation: "converts an iri to a url-safe string.",
    },
    Filter {
        name: "join",
        library: None,
        documentation: "joins a sequence with the argument, like python's `str.join`.",
    },
    Filter {
        name: "json_script",
        library: None,
        documentation: "renders the value as json inside a `<script>` with the given id.",
    },
    Filter {
        name: "last",
        library: None,
        documentation: "the last element.",
    },
    Filter {
        name: "length",
        library: None,
        documentation: "the number of elements.",
    },
    Filter {
        name: "linebreaks",
        library: None,
        documentation: "converts newlines into `<p>` and `<br>`.",
    },
    Filter {
        name: "linebreaksbr",
        library: None,
        documentation: "converts newlines into `<br>`.",
    },
    Filter {
        name: "linenumbers",
        library: None,
        documentation: "prefixes each line with its number.",
    },
    Filter {
        name: "ljust",
        library: None,
        documentation: "left-aligns the value in a field of the given width.",
    },
    Filter {
        name: "lower",
        library: None,
        documentation: "lower-cases the value.",
    },
    Filter {
        name: "make_list",
        library: None,
        documentation: "turns the value into a list of its characters.",
    },
    Filter {
        name: "phone2numeric",
        library: None,
        documentation: "converts a phone number's letters into digits.",
    },
    Filter {
        name: "pluralize",
        library: None,
        documentation: "the plural suffix, when the value is not one.",
    },
    Filter {
        name: "pprint",
        library: None,
        documentation: "pretty-prints the value. for debugging.",
    },
    Filter {
        name: "random",
        library: None,
        documentation: "a random element.",
    },
    Filter {
        name: "rjust",
        library: None,
        documentation: "right-aligns the value in a field of the given width.",
    },
    Filter {
        name: "safe",
        library: None,
        documentation: "marks the value as needing no html escaping.",
    },
    Filter {
        name: "safeseq",
        library: None,
        documentation: "marks each element of a sequence as needing no html escaping.",
    },
    Filter {
        name: "slice",
        library: None,
        documentation: "slices a sequence, with python's slice syntax.",
    },
    Filter {
        name: "slugify",
        library: None,
        documentation: "converts the value into a url slug.",
    },
    Filter {
        name: "stringformat",
        library: None,
        documentation: "formats the value with a python `%` format specifier.",
    },
    Filter {
        name: "striptags",
        library: None,
        documentation: "removes every html tag.",
    },
    Filter {
        name: "time",
        library: None,
        documentation: "formats a time with the given format string.",
    },
    Filter {
        name: "timesince",
        library: None,
        documentation: "the time from the value until now, as `3 days`.",
    },
    Filter {
        name: "timeuntil",
        library: None,
        documentation: "the time from now until the value, as `3 days`.",
    },
    Filter {
        name: "title",
        library: None,
        documentation: "title-cases the value.",
    },
    Filter {
        name: "truncatechars",
        library: None,
        documentation: "truncates to the given number of characters.",
    },
    Filter {
        name: "truncatechars_html",
        library: None,
        documentation: "truncates to the given number of characters, keeping html tags balanced.",
    },
    Filter {
        name: "truncatewords",
        library: None,
        documentation: "truncates to the given number of words.",
    },
    Filter {
        name: "truncatewords_html",
        library: None,
        documentation: "truncates to the given number of words, keeping html tags balanced.",
    },
    Filter {
        name: "unordered_list",
        library: None,
        documentation: "renders a nested list as `<li>` elements.",
    },
    Filter {
        name: "upper",
        library: None,
        documentation: "upper-cases the value.",
    },
    Filter {
        name: "urlencode",
        library: None,
        documentation: "url-encodes the value.",
    },
    Filter {
        name: "urlize",
        library: None,
        documentation: "turns urls in the text into links.",
    },
    Filter {
        name: "urlizetrunc",
        library: None,
        documentation: "turns urls in the text into links, truncating their text.",
    },
    Filter {
        name: "wordcount",
        library: None,
        documentation: "the number of words.",
    },
    Filter {
        name: "wordwrap",
        library: None,
        documentation: "wraps the text at the given width.",
    },
    Filter {
        name: "yesno",
        library: None,
        documentation: "maps `True`, `False` and `None` onto the given words.",
    },
    // `i18n`
    Filter {
        name: "language_name",
        library: Some("i18n"),
        documentation: "the name of the language with the given code.",
    },
    Filter {
        name: "language_name_local",
        library: Some("i18n"),
        documentation: "the name of the language with the given code, in that language.",
    },
    Filter {
        name: "language_bidi",
        library: Some("i18n"),
        documentation: "whether the language with the given code is right-to-left.",
    },
    // `l10n`
    Filter {
        name: "localize",
        library: Some("l10n"),
        documentation: "formats the number with the active locale's conventions.",
    },
    Filter {
        name: "unlocalize",
        library: Some("l10n"),
        documentation: "formats the number without locale conventions.",
    },
    // `tz`
    Filter {
        name: "localtime",
        library: Some("tz"),
        documentation: "converts the datetime to the active time zone.",
    },
    Filter {
        name: "utc",
        library: Some("tz"),
        documentation: "converts the datetime to utc.",
    },
    Filter {
        name: "timezone",
        library: Some("tz"),
        documentation: "converts the datetime to the given time zone.",
    },
];

#[cfg(test)]
mod tests {
    use super::{FILTERS, TAGS, end_tag_for, filter, tag};

    #[test]
    fn tag_names_are_unique() {
        let mut names: Vec<_> = TAGS.iter().map(|tag| tag.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "a tag name is listed twice");
    }

    #[test]
    fn filter_names_are_unique_within_a_library() {
        // `localize` and `timezone` are deliberately both a tag and a filter, and
        // `localtime` is a tag of `tz` as well as a filter of it — but no two
        // *filters* of the same library may share a name
        let mut keys: Vec<_> = FILTERS
            .iter()
            .map(|filter| (filter.library, filter.name))
            .collect();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "a filter name is listed twice");
    }

    #[test]
    fn every_end_tag_belongs_to_exactly_one_block_tag() {
        let mut ends: Vec<_> = TAGS.iter().filter_map(|tag| tag.closed_by).collect();
        ends.sort_unstable();
        let count = ends.len();
        ends.dedup();
        assert_eq!(ends.len(), count, "two block tags share an end tag");
    }

    #[test]
    fn end_tags_are_not_themselves_listed_as_tags() {
        // they are offered from the enclosing block instead, so that
        // `{% endfor %}` is only ever suggested inside a `{% for %}`
        for end in TAGS.iter().filter_map(|tag| tag.closed_by) {
            assert!(tag(end).is_none(), "`{end}` is listed as a tag of its own");
        }
    }

    #[test]
    fn lookup() {
        assert_eq!(end_tag_for("for"), Some("endfor"));
        assert_eq!(end_tag_for("include"), None);
        assert_eq!(end_tag_for("nonexistent"), None);
        assert_eq!(tag("for").map(|tag| tag.branches), Some(&["empty"][..]));
        assert_eq!(
            tag("partialdef").and_then(|tag| tag.closed_by),
            Some("endpartialdef")
        );
        assert_eq!(tag("partial").and_then(|tag| tag.closed_by), None);
        assert_eq!(filter("upper").map(|filter| filter.library), Some(None));
        assert_eq!(tag("static").map(|tag| tag.library), Some(Some("static")));
    }
}

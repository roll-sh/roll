#!/usr/bin/env python3
"""Captures Elasticsearch's `_analyze` output for a matrix of requests as JSON fixtures.

usage: python3 -I gen_fixtures.py <es url> <out dir> [group ...]

Each group writes <out dir>/<group>.json: {"cases": [{"name", "index"?, "request", "status",
"response"}]}, one case per line. A case with "index" first creates an index with those settings
(and "_mappings" if given); a failing index creation is recorded as the case's response
("create": true). A case with "compact": true records a successful response as a digest of its
tokens (see `digest`), and a request text {"$text": key} refers to the file's shared "texts".
`tests/fixtures.rs` replays every case against search-analysis and compares.

The `snowball_vocab` and `stem_vocab` groups read the Snowball test vocabularies (snowball-data)
from $SNOWBALL_VOCAB_DIR (`<language>.txt`); `chains_fuzz` takes $CHAINS_SEED/$CHAINS_COUNT to
explore other samples. Every index the generator creates is named `analysis-fx-*` and deleted.
"""
import json, sys, urllib.request, urllib.error

ES = sys.argv[1].rstrip('/')
OUT = sys.argv[2]
ONLY = set(sys.argv[3:])


def call(method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(ES + path, data=data, method=method, headers={'content-type': 'application/json'})
    try:
        with urllib.request.urlopen(req) as r:
            return r.status, json.loads(r.read() or b'{}')
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b'{}')


TEXTS = {
    'english': "The QUICK brown-fox jumped over the lazy dog's bone, didn't it? Mr. O'Neil's I.B.M. AT&T",
    'punct': "Hello, World! It's 3.14 o'clock; wi-fi Wi-Fi PowerShot SD500 C++ C# x86_64 foo_bar foo.bar",
    'email_url': "e-mail john.doe+tag@example.com or visit https://www.elastic.co/guide?x=1#frag and ftp://a.b/c, www.example.org:8080/x example.com",
    'latin': "Ünïcödé façade naïve Æsir straße İstanbul ISTANBUL Ǆemal ǅ ﬁnancial ŒUVRE",
    'greek': "ΣΊΣΥΦΟΣ σίσυφος Μάϊος ΆΛΦΑ",
    'cjk': "我是中国人。日本語のテキスト、カタカナ、ひらがな。한국어 텍스트 ｶﾀｶﾅ ＦＵＬＬ１２３",
    'emoji': "😀 👍🏽 🇭🇺 👨‍👩‍👧 #️⃣ 1️⃣ ©️ a😀b ☕️x",
    'sea': "ภาษาไทยง่ายนิดเดียว ລາວ ខ្មែរ မြန်မာ",
    'numbers': "1,000.50 $12 v1.2.3 192.168.0.1 2024-01-01 ٣٤٥ ௧௨ 1e10 -5 +7 ½",
    'space': "  leading and trailing   spaces\t\ttabs\nnewlines\r\n",
    'empty': "",
    'single': "a",
    'long': "x" * 300 + " " + "y" * 256 + " z",
    'rtl': "Hebrew שָׁלוֹם \"quoted\" ב\"ה Arabic مرحبا بالعالم ١٢٣ فارسی ۴۵",
    'path': "/usr/local/bin/es-search",
    'mixed_scripts': "Москва Kyiv Київ Беларусь Tbilisi თბილისი Yerevan Երևան हिन्दी বাংলা",
    'apostrophes': "l'avion qu'il j'ai Türkiye'de it’s O’Reilly",
}


def texts(*names):
    return [(n, TEXTS[n]) for n in names] if names else list(TEXTS.items())


GROUPS = {}
# Texts a group's cases refer to as {"$text": key} (written once, as the file's "texts").
SHARED_TEXTS = {}


def group(name):
    def deco(f):
        GROUPS[name] = f
        return f
    return deco


@group('tokenizers')
def tokenizers():
    cases = []
    simple = ['standard', 'letter', 'lowercase', 'whitespace', 'uax_url_email', 'classic', 'keyword', 'ngram', 'edge_ngram',
              'pattern', 'path_hierarchy', 'thai']
    for tk in simple:
        for tn, t in texts():
            if tk in ('ngram', 'edge_ngram') and len(t) > 120:
                continue
            cases.append({'name': f'{tk}/{tn}', 'request': {'tokenizer': tk, 'text': t}})
    configs = [
        ('standard_max5', {'type': 'standard', 'max_token_length': 5}),
        ('standard_max1', {'type': 'standard', 'max_token_length': 1}),
        ('whitespace_max3', {'type': 'whitespace', 'max_token_length': 3}),
        ('classic_max5', {'type': 'classic', 'max_token_length': 5}),
        ('uax_max10', {'type': 'uax_url_email', 'max_token_length': 10}),
        ('ngram_2_3', {'type': 'ngram', 'min_gram': 2, 'max_gram': 3}),
        ('ngram_letters', {'type': 'ngram', 'min_gram': 1, 'max_gram': 2, 'token_chars': ['letter', 'digit']}),
        ('ngram_custom', {'type': 'ngram', 'min_gram': 2, 'max_gram': 3, 'token_chars': ['letter', 'custom'], 'custom_token_chars': "+-_'"}),
        ('ngram_categories', {'type': 'ngram', 'min_gram': 1, 'max_gram': 1, 'token_chars': ['uppercase_letter', 'math_symbol', 'other_letter']}),
        ('edge_2_5', {'type': 'edge_ngram', 'min_gram': 2, 'max_gram': 5}),
        ('edge_letters', {'type': 'edge_ngram', 'min_gram': 1, 'max_gram': 10, 'token_chars': ['letter']}),
        ('edge_punct', {'type': 'edge_ngram', 'min_gram': 1, 'max_gram': 4, 'token_chars': ['punctuation', 'symbol', 'whitespace']}),
        ('pattern_comma', {'type': 'pattern', 'pattern': ','}),
        ('pattern_group', {'type': 'pattern', 'pattern': '"((?:\\\\"|[^"]|\\\\")+)"', 'group': 1}),
        ('pattern_group0', {'type': 'pattern', 'pattern': '[a-z]+', 'group': 0}),
        ('pattern_ci', {'type': 'pattern', 'pattern': 'AND|OR', 'flags': 'CASE_INSENSITIVE'}),
        ('pattern_ucc', {'type': 'pattern', 'pattern': '\\W+', 'flags': 'UNICODE_CHARACTER_CLASS'}),
        ('pattern_camel', {'type': 'pattern', 'pattern': '([^\\p{L}\\d]+)|(?<=\\D)(?=\\d)|(?<=\\d)(?=\\D)'}),
        ('simple_pattern_digits', {'type': 'simple_pattern', 'pattern': '[0123456789]{3}'}),
        ('simple_pattern_words', {'type': 'simple_pattern', 'pattern': '[a-zA-Z]+'}),
        ('simple_pattern_alt', {'type': 'simple_pattern', 'pattern': 'abc|b'}),
        ('simple_pattern_empty', {'type': 'simple_pattern'}),
        ('simple_split_underscore', {'type': 'simple_pattern_split', 'pattern': '_'}),
        ('simple_split_space', {'type': 'simple_pattern_split', 'pattern': '[ ,.]+'}),
        ('simple_split_alt', {'type': 'simple_pattern_split', 'pattern': 'abc|b'}),
        ('char_group_ws', {'type': 'char_group', 'tokenize_on_chars': ['whitespace', '-', '\n']}),
        ('char_group_classes', {'type': 'char_group', 'tokenize_on_chars': ['punctuation', 'symbol', 'digit']}),
        ('char_group_escapes', {'type': 'char_group', 'tokenize_on_chars': ['\\u0020', '\\t', '\\n', 'letter']}),
        ('char_group_max', {'type': 'char_group', 'tokenize_on_chars': ['whitespace'], 'max_token_length': 4}),
        ('path_dash', {'type': 'path_hierarchy', 'delimiter': '-', 'replacement': '/', 'skip': 1}),
        ('path_reverse', {'type': 'path_hierarchy', 'reverse': True}),
        ('path_reverse_skip', {'type': 'path_hierarchy', 'delimiter': '.', 'reverse': True, 'skip': 1}),
        ('path_skip2', {'type': 'path_hierarchy', 'skip': 2}),
        ('keyword_buf', {'type': 'keyword', 'buffer_size': 2}),
    ]
    extra_texts = {
        'domains': 'www.example.com a.b.c one-two-three /a/b/c 2015-01-02 foo_bar_baz __a__b_ fd-786-335-514-x abab xab',
    }
    for cn, cfg in configs:
        for tn, t in list(texts('english', 'punct', 'latin', 'cjk', 'emoji', 'path', 'empty', 'numbers', 'apostrophes')) + list(extra_texts.items()):
            cases.append({'name': f'{cn}/{tn}', 'request': {'tokenizer': cfg, 'text': t}})
    # Multi-valued text: offsets and positions continue across values.
    cases.append({'name': 'standard/multi', 'request': {'tokenizer': 'standard', 'text': ['one two', 'three', '', 'four']}})
    cases.append({'name': 'standard/multi_explain', 'request': {'tokenizer': 'standard', 'text': ['one two', 'three'], 'explain': True}})
    cases.append({'name': 'standard_analyzer/multi', 'request': {'analyzer': 'standard', 'text': ['one two', 'three']}})
    return cases


@group('analyzers')
def analyzers():
    cases = []
    names = ['standard', 'simple', 'whitespace', 'stop', 'keyword', 'pattern', 'fingerprint', 'classic', 'snowball', 'chinese', 'cjk',
             'arabic', 'armenian', 'basque', 'bengali', 'brazilian', 'bulgarian', 'catalan', 'czech', 'danish', 'dutch', 'english',
             'estonian', 'finnish', 'french', 'galician', 'german', 'greek', 'hindi', 'hungarian', 'indonesian', 'irish', 'italian',
             'latvian', 'lithuanian', 'norwegian', 'persian', 'portuguese', 'romanian', 'russian', 'serbian', 'sorani', 'spanish',
             'swedish', 'turkish', 'thai']
    for a in names:
        for tn, t in texts('english', 'punct', 'latin', 'cjk', 'emoji', 'rtl', 'mixed_scripts', 'apostrophes', 'numbers'):
            cases.append({'name': f'{a}/{tn}', 'request': {'analyzer': a, 'text': t}})
        cases.append({'name': f'{a}/explain', 'request': {'analyzer': a, 'text': TEXTS['english'], 'explain': True}})
    return cases


@group('filters')
def filters():
    cases = []
    by_name = ['lowercase', 'uppercase', 'asciifolding', 'stop', 'trim', 'truncate', 'reverse', 'unique', 'remove_duplicates',
               'apostrophe', 'classic', 'decimal_digit', 'elision', 'length', 'limit', 'keyword_repeat', 'kstem', 'porter_stem',
               'snowball', 'stemmer', 'scandinavian_folding', 'scandinavian_normalization', 'cjk_width', 'cjk_bigram',
               'arabic_normalization', 'arabic_stem', 'bengali_normalization', 'brazilian_stem', 'czech_stem', 'dutch_stem',
               'french_stem', 'german_normalization', 'german_stem', 'hindi_normalization', 'indic_normalization',
               'persian_normalization', 'persian_stem', 'russian_stem', 'serbian_normalization', 'sorani_normalization', 'ngram',
               'edge_ngram', 'shingle', 'word_delimiter', 'word_delimiter_graph', 'flatten_graph', 'delimited_payload',
               'common_grams', 'fingerprint', 'min_hash', 'multiplexer']
    for f in by_name:
        for tn, t in texts('english', 'punct', 'latin', 'cjk', 'rtl', 'numbers', 'mixed_scripts', 'apostrophes', 'empty'):
            cases.append({'name': f'{f}/{tn}', 'request': {'tokenizer': 'standard', 'filter': [f], 'text': t}})
        cases.append({'name': f'{f}/ws_explain', 'request': {'tokenizer': 'whitespace', 'filter': [f], 'text': TEXTS['punct'], 'explain': True}})
    return cases


@group('filter_configs')
def filter_configs():
    cases = []
    cfgs = [
        ('lowercase_greek', {'type': 'lowercase', 'language': 'greek'}),
        ('lowercase_irish', {'type': 'lowercase', 'language': 'irish'}),
        ('lowercase_turkish', {'type': 'lowercase', 'language': 'turkish'}),
        ('asciifolding_preserve', {'type': 'asciifolding', 'preserve_original': True}),
        ('stop_english_ic', {'type': 'stop', 'stopwords': '_english_', 'ignore_case': True}),
        ('stop_list', {'type': 'stop', 'stopwords': ['and', 'the', 'is', 'It']}),
        ('stop_none', {'type': 'stop', 'stopwords': '_none_'}),
        ('stop_french', {'type': 'stop', 'stopwords': ['_french_', 'xyz']}),
        ('stop_keep_trailing', {'type': 'stop', 'remove_trailing': False}),
        ('truncate_3', {'type': 'truncate', 'length': 3}),
        ('length_2_4', {'type': 'length', 'min': 2, 'max': 4}),
        ('limit_3', {'type': 'limit', 'max_token_count': 3}),
        ('unique_same_pos', {'type': 'unique', 'only_on_same_position': True}),
        ('keep_words', {'type': 'keep', 'keep_words': ['the', 'QUICK', 'fox']}),
        ('keep_words_case', {'type': 'keep', 'keep_words': ['the', 'QUICK', 'fox'], 'keep_words_case': True}),
        ('keep_types_num', {'type': 'keep_types', 'types': ['<NUM>']}),
        ('keep_types_exclude', {'type': 'keep_types', 'types': ['<NUM>', '<ALPHANUM>'], 'mode': 'exclude'}),
        ('keyword_marker', {'type': 'keyword_marker', 'keywords': ['jumped', 'lazy']}),
        ('keyword_marker_pattern', {'type': 'keyword_marker', 'keywords_pattern': '[a-z]+ed'}),
        ('elision_custom', {'type': 'elision', 'articles': ['l', 'qu'], 'articles_case': True}),
        ('delimited_payload_int', {'type': 'delimited_payload', 'encoding': 'int', 'delimiter': '+'}),
        ('delimited_payload_identity', {'type': 'delimited_payload', 'encoding': 'identity'}),
        ('fingerprint_sep', {'type': 'fingerprint', 'separator': '+', 'max_output_size': 30}),
        ('pattern_replace', {'type': 'pattern_replace', 'pattern': '(\\d+)', 'replacement': '<$1>'}),
        ('pattern_replace_first', {'type': 'pattern_replace', 'pattern': '[aeiou]', 'replacement': '', 'all': False}),
        ('pattern_capture', {'type': 'pattern_capture', 'patterns': ['(\\w+)@(\\w+)', '(\\p{Ll}+)'], 'preserve_original': True}),
        ('pattern_capture_no_orig', {'type': 'pattern_capture', 'patterns': ['([a-z]+)(\\d+)'], 'preserve_original': False}),
        ('ngram_2_3', {'type': 'ngram', 'min_gram': 2, 'max_gram': 3}),
        ('ngram_preserve', {'type': 'ngram', 'min_gram': 3, 'max_gram': 3, 'preserve_original': True}),
        ('edge_ngram_1_4', {'type': 'edge_ngram', 'min_gram': 1, 'max_gram': 4}),
        ('edge_ngram_preserve', {'type': 'edge_ngram', 'min_gram': 2, 'max_gram': 3, 'preserve_original': True}),
        ('edge_ngram_back', {'type': 'edge_ngram', 'min_gram': 1, 'max_gram': 3, 'side': 'back'}),
        ('shingle_2_3', {'type': 'shingle', 'min_shingle_size': 2, 'max_shingle_size': 3}),
        ('shingle_no_unigrams', {'type': 'shingle', 'output_unigrams': False}),
        ('shingle_filler', {'type': 'shingle', 'token_separator': '_', 'filler_token': 'X'}),
        ('shingle_unigrams_if_no', {'type': 'shingle', 'output_unigrams': False, 'output_unigrams_if_no_shingles': True}),
        ('common_grams', {'type': 'common_grams', 'common_words': ['the', 'is', 'a', 'of']}),
        ('common_grams_query', {'type': 'common_grams', 'common_words': ['the', 'is', 'a', 'of'], 'query_mode': True}),
        ('common_grams_ic', {'type': 'common_grams', 'common_words': '_english_', 'ignore_case': True}),
        ('word_delimiter_all', {'type': 'word_delimiter', 'catenate_all': True, 'preserve_original': True}),
        ('word_delimiter_catenate', {'type': 'word_delimiter', 'catenate_words': True, 'catenate_numbers': True, 'generate_number_parts': False}),
        ('word_delimiter_no_split', {'type': 'word_delimiter', 'split_on_case_change': False, 'split_on_numerics': False, 'stem_english_possessive': False}),
        ('word_delimiter_types', {'type': 'word_delimiter', 'type_table': ['+ => ALPHA', '# => ALPHANUM', '. => DIGIT']}),
        ('word_delimiter_protected', {'type': 'word_delimiter', 'protected_words': ['wi-fi', 'SD500']}),
        ('wdg_all', {'type': 'word_delimiter_graph', 'catenate_all': True, 'preserve_original': True}),
        ('wdg_catenate', {'type': 'word_delimiter_graph', 'catenate_words': True, 'catenate_numbers': True}),
        ('wdg_adjust', {'type': 'word_delimiter_graph', 'adjust_offsets': False, 'catenate_words': True}),
        ('wdg_ignore_kw', {'type': 'word_delimiter_graph', 'ignore_keywords': True}),
        ('stemmer_light_english', {'type': 'stemmer', 'language': 'light_english'}),
        ('stemmer_minimal_english', {'type': 'stemmer', 'language': 'minimal_english'}),
        ('stemmer_possessive', {'type': 'stemmer', 'language': 'possessive_english'}),
        ('stemmer_porter2', {'type': 'stemmer', 'language': 'porter2'}),
        ('stemmer_lovins', {'type': 'stemmer', 'language': 'lovins'}),
        ('stemmer_override', {'type': 'stemmer_override', 'rules': ['running, runs => run', 'jumped => jump']}),
        ('snowball_german', {'type': 'snowball', 'language': 'German'}),
        ('snowball_porter', {'type': 'snowball', 'language': 'Porter'}),
        ('min_hash_small', {'type': 'min_hash', 'hash_count': 1, 'bucket_count': 4, 'hash_set_size': 1, 'with_rotation': True}),
        ('min_hash_nrot', {'type': 'min_hash', 'hash_count': 2, 'bucket_count': 2, 'hash_set_size': 2, 'with_rotation': False}),
        ('multiplexer', {'type': 'multiplexer', 'filters': ['lowercase', 'uppercase', 'reverse, lowercase']}),
        ('multiplexer_no_orig', {'type': 'multiplexer', 'filters': ['lowercase'], 'preserve_original': False}),
        ('cjk_bigram_ignore', {'type': 'cjk_bigram', 'ignored_scripts': ['hangul', 'hiragana']}),
        ('cjk_bigram_unigrams', {'type': 'cjk_bigram', 'output_unigrams': True}),
        ('synonym_inline', {'type': 'synonym', 'synonyms': ['quick, fast', 'brown fox => fox', 'lazy, idle, sleepy']}),
        ('synonym_graph_inline', {'type': 'synonym_graph', 'synonyms': ['quick, fast', 'brown fox => fox', 'dog, domestic canine']}),
        ('synonym_no_expand', {'type': 'synonym', 'synonyms': ['quick, fast, speedy'], 'expand': False}),
        ('synonym_wordnet', {'type': 'synonym', 'format': 'wordnet', 'synonyms': ["s(100000001,1,'quick',v,1,0).", "s(100000001,2,'fast',v,1,0).", "s(100000001,3,'speedy',v,1,0)."]}),
        ('synonym_graph_multi', {'type': 'synonym_graph', 'synonyms': ['ny, new york, new york city', 'usa, united states']}),
    ]
    for cn, cfg in cfgs:
        for tn, t in list(texts('english', 'punct', 'latin', 'cjk', 'numbers', 'apostrophes')) + [('syn', 'the quick brown fox and the lazy dog in new york ny usa'), ('payload', 'a|1.5 b+7 c|x d e|')]:
            cases.append({'name': f'{cn}/{tn}', 'request': {'tokenizer': 'whitespace' if tn == 'payload' else 'standard', 'filter': ['lowercase', cfg] if cn.startswith('synonym') else [cfg], 'text': t}})
        cases.append({'name': f'{cn}/explain', 'request': {'tokenizer': 'standard', 'filter': [cfg], 'text': TEXTS['punct'], 'explain': True}})
    return cases


@group('char_filters')
def char_filters():
    cases = []
    cfgs = [
        ('html_strip', 'html_strip'),
        ('html_escaped', {'type': 'html_strip', 'escaped_tags': ['b', 'i']}),
        ('mapping', {'type': 'mapping', 'mappings': ['٠ => 0', '١ => 1', '٢ => 2', ':) => _happy_', ':( => _sad_', 'ph => f', 'a => ', '\\u0020 => _']}),
        ('pattern_replace', {'type': 'pattern_replace', 'pattern': '(\\d+)-(?=\\d)', 'replacement': '$1_'}),
        ('pattern_replace_shrink', {'type': 'pattern_replace', 'pattern': '\\s+', 'replacement': ' '}),
        ('pattern_replace_grow', {'type': 'pattern_replace', 'pattern': 'o', 'replacement': 'OOO'}),
    ]
    html = [
        ('basic', '<p>I&apos;m so <b>happy</b>!</p>'),
        ('entities', 'caf&eacute; &lt;b&gt; &amp;amp; &#169; &#x263A; &nbsp;x &bogus; &#;'),
        ('script', '<html><head><title>T</title><script>var a = "<b>";</script><style>p{}</style></head><body>Hi<br/>there<!-- comment -->!</body></html>'),
        ('attrs', '<a href="http://x.com/?a=1&amp;b=2" title=\'q\'>link</a><img src=x.png alt="pic"> after'),
        ('cdata', '<![CDATA[ raw <b>text</b> ]]> <?php echo 1; ?> <!DOCTYPE html> end'),
        ('broken', '<b>unclosed <i>tags < 3 > 2 <not a tag'),
        ('blocks', '<div>one</div><div>two</div><p>three<br>four</p><li>five</li>'),
    ]
    for cn, cfg in cfgs:
        for tn, t in html + list(texts('english', 'numbers', 'emoji', 'rtl', 'space')) + [('mapping', ':) ph٠١٢ a :( x y')]:
            cases.append({'name': f'{cn}/{tn}', 'request': {'tokenizer': 'standard', 'char_filter': [cfg], 'text': t}})
            cases.append({'name': f'{cn}/{tn}/keyword', 'request': {'tokenizer': 'keyword', 'char_filter': [cfg], 'text': t}})
        cases.append({'name': f'{cn}/explain', 'request': {'tokenizer': 'standard', 'char_filter': [cfg], 'text': html[0][1], 'explain': True}})
    cases.append({'name': 'chain/explain', 'request': {'tokenizer': 'standard', 'char_filter': ['html_strip', {'type': 'mapping', 'mappings': ['happy => sad']}], 'filter': ['lowercase'], 'text': ['<b>Happy</b> days', 'x'], 'explain': True}})
    return cases


@group('errors')
def errors():
    cases = []
    bad = [
        ('unknown_tokenizer', {'tokenizer': 'nope', 'text': 'x'}),
        ('unknown_tokenizer_type', {'tokenizer': {'type': 'nope'}, 'text': 'x'}),
        ('missing_type', {'tokenizer': {'pattern': 'x'}, 'text': 'x'}),
        ('unknown_filter', {'tokenizer': 'standard', 'filter': ['nope'], 'text': 'x'}),
        ('requires_settings', {'tokenizer': 'standard', 'filter': ['synonym'], 'text': 'x'}),
        ('normalizer_filter', {'filter': ['stemmer'], 'text': 'x'}),
        ('normalizer_ok', {'filter': ['lowercase', 'asciifolding'], 'char_filter': ['html_strip'], 'text': '<b>ÀB</b> C'}),
        ('unknown_analyzer', {'analyzer': 'nope', 'text': 'x'}),
        ('no_text', {'analyzer': 'standard'}),
        ('analyzer_and_tokenizer', {'analyzer': 'standard', 'tokenizer': 'standard', 'text': 'x'}),
        ('normalizer_no_index', {'normalizer': 'lowercase', 'text': 'x'}),
        ('field_no_index', {'field': 'f', 'text': 'x'}),
        ('ngram_diff', {'tokenizer': {'type': 'ngram', 'min_gram': 1, 'max_gram': 5}, 'text': 'x'}),
        ('ngram_min_gt_max', {'tokenizer': {'type': 'edge_ngram', 'min_gram': 5, 'max_gram': 2}, 'text': 'x'}),
        ('ngram_bad_int', {'tokenizer': {'type': 'ngram', 'min_gram': 'abc'}, 'text': 'x'}),
        ('token_chars_bad', {'tokenizer': {'type': 'ngram', 'token_chars': ['nope']}, 'text': 'x'}),
        ('token_chars_custom', {'tokenizer': {'type': 'ngram', 'token_chars': ['custom']}, 'text': 'x'}),
        ('stop_path', {'tokenizer': 'standard', 'filter': [{'type': 'stop', 'stopwords_path': 'stop.txt'}], 'text': 'x'}),
        ('synonyms_path', {'tokenizer': 'standard', 'filter': [{'type': 'synonym', 'synonyms_path': 'analysis/syn.txt'}], 'text': 'x'}),
        ('mapping_path', {'tokenizer': 'standard', 'char_filter': [{'type': 'mapping', 'mappings_path': 'm.txt'}], 'text': 'x'}),
        ('mapping_missing', {'tokenizer': 'standard', 'char_filter': [{'type': 'mapping'}], 'text': 'x'}),
        ('bad_bool', {'tokenizer': 'standard', 'filter': [{'type': 'stop', 'ignore_case': 'yes'}], 'text': 'x'}),
        ('lowercase_lang', {'tokenizer': 'standard', 'filter': [{'type': 'lowercase', 'language': 'klingon'}], 'text': 'x'}),
        ('truncate_no_length', {'tokenizer': 'standard', 'filter': [{'type': 'truncate'}], 'text': 'x'}),
        ('keep_none', {'tokenizer': 'standard', 'filter': [{'type': 'keep'}], 'text': 'x'}),
        ('keep_types_mode', {'tokenizer': 'standard', 'filter': [{'type': 'keep_types', 'types': ['a'], 'mode': 'x'}], 'text': 'x'}),
        ('char_group_bad', {'tokenizer': {'type': 'char_group', 'tokenize_on_chars': ['abc']}, 'text': 'x'}),
        ('pattern_bad_group', {'tokenizer': {'type': 'pattern', 'pattern': 'a(b)', 'group': 2}, 'text': 'x'}),
        ('pattern_bad_flag', {'tokenizer': {'type': 'pattern', 'pattern': 'a', 'flags': 'NOPE'}, 'text': 'x'}),
        ('path_delim', {'tokenizer': {'type': 'path_hierarchy', 'delimiter': 'ab'}, 'text': 'x'}),
        ('standard_filter', {'tokenizer': 'standard', 'filter': ['standard'], 'text': 'x'}),
        ('nGram_tokenizer', {'tokenizer': 'nGram', 'text': 'x'}),
        ('edgeNGram_filter', {'tokenizer': 'standard', 'filter': [{'type': 'edgeNGram'}], 'text': 'x'}),
        ('shingle_diff', {'tokenizer': 'standard', 'filter': [{'type': 'shingle', 'max_shingle_size': 10}], 'text': 'x'}),
        ('shingle_min', {'tokenizer': 'standard', 'filter': [{'type': 'shingle', 'min_shingle_size': 1}], 'text': 'x'}),
        ('stemmer_lang', {'tokenizer': 'standard', 'filter': [{'type': 'stemmer', 'language': 'klingon'}], 'text': 'x'}),
        ('synonym_bad_rule', {'tokenizer': 'standard', 'filter': [{'type': 'synonym', 'synonyms': ['a => ']}], 'text': 'x'}),
        ('synonym_lenient', {'tokenizer': 'standard', 'filter': [{'type': 'synonym', 'synonyms': ['a => ', 'b, c'], 'lenient': True}], 'text': 'a b'}),
        ('synonym_after_ngram', {'tokenizer': 'standard', 'filter': ['ngram', {'type': 'synonym', 'synonyms': ['a, b']}], 'text': 'x'}),
        ('condition_no_script', {'tokenizer': 'standard', 'filter': [{'type': 'condition', 'filter': ['lowercase']}], 'text': 'x'}),
        ('multiplexer_unknown', {'tokenizer': 'standard', 'filter': [{'type': 'multiplexer', 'filters': ['nope']}], 'text': 'x'}),
        ('payload_bad_float', {'tokenizer': 'whitespace', 'filter': ['delimited_payload'], 'text': 'a|xyz'}),
        ('explain_attributes', {'tokenizer': 'standard', 'filter': ['porter_stem'], 'text': 'running dogs', 'explain': True, 'attributes': ['keyword']}),
        ('explain_attributes_none', {'tokenizer': 'standard', 'filter': ['porter_stem'], 'text': 'running dogs', 'explain': True, 'attributes': ['nope']}),
    ]
    for n, r in bad:
        cases.append({'name': n, 'request': r})
    return cases


@group('index')
def index():
    cases = []
    settings_cases = [
        ('custom_analyzer', {'analysis': {
            'char_filter': {'emoticons': {'type': 'mapping', 'mappings': [':) => _happy_']}},
            'tokenizer': {'punct': {'type': 'pattern', 'pattern': '[ .,!?]'}},
            'filter': {'en_stop': {'type': 'stop', 'stopwords': '_english_'}, 'my_stem': {'type': 'stemmer', 'language': 'english'}},
            'analyzer': {'my': {'type': 'custom', 'char_filter': ['emoticons'], 'tokenizer': 'punct', 'filter': ['lowercase', 'en_stop', 'my_stem'], 'position_increment_gap': 10}}}},
         [{'analyzer': 'my', 'text': ["I'm a :) person, and you?", 'Running dogs']},
          {'analyzer': 'my', 'text': ["I'm a :) person", 'second value'], 'explain': True},
          {'tokenizer': 'punct', 'filter': ['en_stop'], 'text': 'the cat,the dog'},
          {'analyzer': 'standard', 'text': 'Default Standard'},
          {'text': 'Uses Default'}]),
        ('default_analyzer', {'analysis': {'analyzer': {'default': {'type': 'simple'}}}},
         [{'text': 'Simple 123 Analyzer'}]),
        ('normalizer', {'analysis': {'normalizer': {'n': {'type': 'custom', 'char_filter': [], 'filter': ['lowercase', 'asciifolding']}}}},
         [{'normalizer': 'n', 'text': 'ÀÉÎ Õü'}, {'normalizer': 'n', 'text': ['ÀÉÎ Õü', 'B'], 'explain': True}, {'normalizer': 'lowercase', 'text': 'ABC def'}]),
        ('language_custom', {'analysis': {'analyzer': {
            'en': {'type': 'english', 'stopwords': ['jumped'], 'stem_exclusion': ['running']},
            'fr': {'type': 'french', 'stopwords': '_none_'},
            'std': {'type': 'standard', 'stopwords': '_english_', 'max_token_length': 4},
            'pat': {'type': 'pattern', 'pattern': '\\s+', 'lowercase': False},
            'fp': {'type': 'fingerprint', 'stopwords': '_english_', 'separator': '-'},
            'st': {'type': 'stop', 'stopwords': ['foo']},
            'snow': {'type': 'snowball', 'language': 'Dutch'}}}},
         [{'analyzer': a, 'text': "The running dogs jumped over L'église and foo Fietsen Bromfietsen"} for a in ['en', 'fr', 'std', 'pat', 'fp', 'st', 'snow']]),
        ('ngram_diff_ok', {'index': {'max_ngram_diff': 5}, 'analysis': {'tokenizer': {'ng': {'type': 'ngram', 'min_gram': 1, 'max_gram': 4}},
                                                                       'filter': {'ngf': {'type': 'ngram', 'min_gram': 1, 'max_gram': 5}}}},
         [{'tokenizer': 'ng', 'text': 'hello'}, {'tokenizer': 'standard', 'filter': ['ngf'], 'text': 'hello'}]),
        ('synonyms_graph_search', {'analysis': {'filter': {'syn': {'type': 'synonym_graph', 'synonyms': ['ny, new york', 'dns => domain name system']}},
                                                'analyzer': {'s': {'tokenizer': 'standard', 'filter': ['lowercase', 'syn']}}}},
         [{'analyzer': 's', 'text': 'new york DNS lookup in NY'}, {'analyzer': 's', 'text': 'ny dns', 'explain': True}]),
        ('multiplexer_condition', {'analysis': {'filter': {'mx': {'type': 'multiplexer', 'filters': ['lowercase', 'my_stem, reverse']},
                                                           'my_stem': {'type': 'stemmer', 'language': 'english'}},
                                                'analyzer': {'m': {'tokenizer': 'standard', 'filter': ['mx']}}}},
         [{'analyzer': 'm', 'text': 'Running Dogs'}, {'analyzer': 'm', 'text': 'Running Dogs', 'explain': True}]),
        ('field_analyzer', {'analysis': {'analyzer': {'a': {'type': 'whitespace'}}}, '_mappings': {'properties': {'f': {'type': 'text', 'analyzer': 'a'}}}},
         [{'field': 'f', 'text': 'Whitespace Only-Here'}, {'field': 'unknown', 'text': 'Falls Back'}]),
        ('nested_dots', {'analysis.analyzer.d.tokenizer': 'keyword', 'analysis.analyzer.d.filter': ['lowercase']},
         [{'analyzer': 'd', 'text': 'Dotted Keys'}]),
        ('updateable_synonyms', {'analysis': {'filter': {'u': {'type': 'synonym', 'synonyms': ['a, b'], 'updateable': True}},
                                              'analyzer': {'ua': {'tokenizer': 'standard', 'filter': ['u']}}}},
         [{'analyzer': 'ua', 'text': 'a c'}]),
    ]
    for name, settings, reqs in settings_cases:
        for i, r in enumerate(reqs):
            cases.append({'name': f'{name}/{i}', 'index': settings, 'request': r})
    failing = [
        ('unknown_tokenizer', {'analysis': {'analyzer': {'a': {'tokenizer': 'nope'}}}}),
        ('unknown_filter_type', {'analysis': {'filter': {'f': {'type': 'nope'}}}}),
        ('missing_filter_type', {'analysis': {'filter': {'f': {'stopwords': ['a']}}}}),
        ('analyzer_no_tokenizer', {'analysis': {'analyzer': {'a': {'filter': ['lowercase']}}}}),
        ('custom_no_tokenizer', {'analysis': {'analyzer': {'a': {'type': 'custom', 'filter': ['lowercase']}}}}),
        ('unknown_analyzer_type', {'analysis': {'analyzer': {'a': {'type': 'nope'}}}}),
        ('unknown_filter_ref', {'analysis': {'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['nope']}}}}),
        ('unknown_char_filter_ref', {'analysis': {'analyzer': {'a': {'tokenizer': 'standard', 'char_filter': ['nope']}}}}),
        ('normalizer_bad_filter', {'analysis': {'normalizer': {'n': {'filter': ['stemmer']}}}}),
        ('normalizer_tokenizer', {'analysis': {'normalizer': {'n': {'tokenizer': 'standard'}}}}),
        ('normalizer_unknown_filter', {'analysis': {'normalizer': {'n': {'filter': ['nope']}}}}),
        ('normalizer_bad_char_filter', {'analysis': {'normalizer': {'n': {'char_filter': ['html_strip']}}}}),
        ('normalizer_unknown_type', {'analysis': {'normalizer': {'n': {'type': 'nope'}}}}),
        ('underscore_name', {'analysis': {'analyzer': {'_a': {'tokenizer': 'standard'}}}}),
        ('stop_path', {'analysis': {'filter': {'s': {'type': 'stop', 'stopwords_path': 'stopwords/x.txt'}}}}),
        ('bad_int', {'analysis': {'tokenizer': {'t': {'type': 'standard', 'max_token_length': 'x'}}}}),
        ('max_token_length_zero', {'analysis': {'tokenizer': {'t': {'type': 'whitespace', 'max_token_length': 0}}, 'analyzer': {'a': {'tokenizer': 't'}}}}),
        ('ngram_diff', {'analysis': {'tokenizer': {'t': {'type': 'ngram', 'min_gram': 1, 'max_gram': 3}}}}),
        ('synonym_bad', {'analysis': {'filter': {'s': {'type': 'synonym', 'synonyms': ['a b => ']}}, 'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['s']}}}}),
        ('synonym_unused_bad', {'analysis': {'filter': {'s': {'type': 'synonym', 'synonyms': ['a => ']}}}}),
        ('updateable_index_time', {'analysis': {'filter': {'u': {'type': 'synonym', 'synonyms': ['a, b'], 'updateable': True}},
                                               'analyzer': {'default': {'tokenizer': 'standard', 'filter': ['u']}}}}),
        ('nGram_filter_used', {'analysis': {'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['nGram']}}}}),
        ('edgeNGram_tokenizer_type', {'analysis': {'tokenizer': {'t': {'type': 'edgeNGram'}}}}),
        ('default_index', {'analysis': {'analyzer': {'default_index': {'type': 'standard'}}}}),
        ('shingle_diff', {'analysis': {'filter': {'s': {'type': 'shingle', 'max_shingle_size': 8}}}}),
        ('word_delimiter_bad_type', {'analysis': {'filter': {'w': {'type': 'word_delimiter', 'type_table': ['a => NOPE']}}}}),
        ('multiplexer_bad', {'analysis': {'filter': {'m': {'type': 'multiplexer', 'filters': ['nope']}}, 'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['m']}}}}),
    ]
    for name, settings in failing:
        cases.append({'name': f'fail/{name}', 'index': settings, 'request': {'text': 'x'}})
    return cases


STEM_TEXTS = {
    'english': "The runners were running quickly; ponies, flies and cities' hopefulness generalizations dogs' bones",
    'french': "Les chevaux étaient rapidement entrés dans les maisons; nationalisation généralement heureusement",
    'german': "Die Häuser der Mädchen wurden schöner; Aufeinanderfolgenden Freundschaften laufend",
    'spanish': "Los niños corrían rápidamente por las calles; canciones naciones felicidades voces",
    'italian': "I ragazzi correvano velocemente nelle città; nazionalizzazione felicemente amiche",
    'portuguese': "Os meninos corriam rapidamente pelas ruas; canções nações felicidades papéis lençóis",
    'dutch': "De kinderen liepen snel door de straten; fietsen bromfietsen gelukkige vriendelijkheid",
    'swedish': "Barnen sprang snabbt genom gatorna; vänligheten flickornas husen",
    'norwegian': "Barna løp raskt gjennom gatene; hemmeligheter vennligheten jentene husene",
    'danish': "Børnene løb hurtigt gennem gaderne; venligheden pigernes husene",
    'finnish': "Lapset juoksivat nopeasti kaduilla; ystävällisyys taloissa kirjoittamassa",
    'hungarian': "A gyerekek gyorsan futottak az utcákon; barátságos házakban könyveinkkel",
    'russian': "Дети быстро бежали по улицам; красивыми домами книгами",
    'czech': "Děti rychle běžely ulicemi; krásnými domy knihami kočkách",
    'bulgarian': "Децата тичаха бързо по улиците; красивите къщи книгите",
    'latvian': "Bērni ātri skrēja pa ielām; skaistajām mājām grāmatām",
    'lithuanian': "Vaikai greitai bėgo gatvėmis; gražiais namais knygomis",
    'greek': "Τα παιδιά έτρεχαν γρήγορα στους δρόμους; καθεστώτων γεγονότα ολογιών",
    'hindi': "बच्चे सड़कों पर तेजी से दौड़े; किताबें लड़कियाँ जाएंगे",
    'bengali': "শিশুরা দ্রুত রাস্তায় দৌড়েছিল; বইগুলো মেয়েদেরকে করিতেছিলাম",
    'arabic': "الأطفال ركضوا بسرعة في الشوارع؛ والكتاب بالمدرسة كتابها",
    'persian': "کودکان به سرعت در خیابان‌ها دویدند؛ کتاب‌ها بزرگترین",
    'sorani': "منداڵەکان بە خێرایی ڕۆیشتن؛ کتێبەکان ماڵەکەمان",
    'indonesian': "Anak-anak berlari dengan cepat di jalanan; mempermainkan pembacaan kebersihan",
    'irish': "Rith na páistí go tapa ar na sráideanna; n-athair t-uisce h-oileán",
    'basque': "Haurrak azkar korrika joan ziren kaleetan; etxeetako liburuekin",
    'catalan': "Els nens corrien ràpidament pels carrers; l'home d'aquí cançons",
    'armenian': "Երեխաները արագ վազում էին փողոցներով; գրքերը տներից",
    'estonian': "Lapsed jooksid kiiresti mööda tänavaid; ilusatest majadest raamatutega",
    'turkish': "Çocuklar sokaklarda hızla koştular; kitaplarımızdan evlerinde İstanbul'da",
    'romanian': "Copiii alergau repede pe străzi; frumoaselor case cărţile ştiinţă",
    'serbian': "Деца су брзо трчала улицама; djeca lijepim kućama čokolada",
    'galician': "Os nenos corrían rapidamente polas rúas; cancións nacións felicidades",
}

STEMMER_LANGS = {
    'arabic': 'arabic', 'armenian': 'armenian', 'basque': 'basque', 'bengali': 'bengali', 'brazilian': 'portuguese',
    'bulgarian': 'bulgarian', 'catalan': 'catalan', 'czech': 'czech', 'danish': 'danish', 'dutch': 'dutch',
    'dutch_kp': 'dutch', 'english': 'english', 'light_english': 'english', 'lovins': 'english', 'minimal_english': 'english',
    'porter2': 'english', 'possessive_english': 'english', 'estonian': 'estonian', 'finnish': 'finnish',
    'light_finnish': 'finnish', 'french': 'french', 'light_french': 'french', 'minimal_french': 'french',
    'galician': 'galician', 'minimal_galician': 'galician', 'german': 'german', 'german2': 'german',
    'light_german': 'german', 'minimal_german': 'german', 'greek': 'greek', 'hindi': 'hindi', 'hungarian': 'hungarian',
    'light_hungarian': 'hungarian', 'indonesian': 'indonesian', 'irish': 'irish', 'italian': 'italian',
    'light_italian': 'italian', 'sorani': 'sorani', 'latvian': 'latvian', 'lithuanian': 'lithuanian',
    'norwegian': 'norwegian', 'light_norwegian': 'norwegian', 'minimal_norwegian': 'norwegian',
    'light_nynorsk': 'norwegian', 'minimal_nynorsk': 'norwegian', 'persian': 'persian', 'portuguese': 'portuguese',
    'light_portuguese': 'portuguese', 'minimal_portuguese': 'portuguese', 'portuguese_rslp': 'portuguese',
    'romanian': 'romanian', 'russian': 'russian', 'light_russian': 'russian', 'serbian': 'serbian',
    'spanish': 'spanish', 'light_spanish': 'spanish', 'spanish_plural': 'spanish', 'swedish': 'swedish',
    'light_swedish': 'swedish', 'turkish': 'turkish', 'porter': 'english', 'kstem': 'english', 'lightGerman': 'german',
}


@group('stemmers')
def stemmers():
    cases = []
    for lang, text_lang in STEMMER_LANGS.items():
        cases.append({'name': f'stemmer/{lang}', 'request': {'tokenizer': 'standard', 'filter': ['lowercase', {'type': 'stemmer', 'language': lang}], 'text': STEM_TEXTS[text_lang]}})
    for lang in ['English', 'Porter', 'German', 'German2', 'Dutch', 'Kp', 'Lovins', 'Russian', 'Estonian', 'Turkish', 'Hindi', 'Greek', 'Nepali', 'Tamil', 'Yiddish', 'Klingon']:
        text = STEM_TEXTS.get(lang.lower(), STEM_TEXTS['english'])
        cases.append({'name': f'snowball/{lang}', 'request': {'tokenizer': 'standard', 'filter': ['lowercase', {'type': 'snowball', 'language': lang}], 'text': text}})
    for f, lang in [('arabic_stem', 'arabic'), ('brazilian_stem', 'portuguese'), ('czech_stem', 'czech'), ('dutch_stem', 'dutch'),
                    ('french_stem', 'french'), ('german_stem', 'german'), ('persian_stem', 'persian'), ('russian_stem', 'russian'),
                    ('arabic_normalization', 'arabic'), ('persian_normalization', 'persian'), ('sorani_normalization', 'sorani'),
                    ('hindi_normalization', 'hindi'), ('bengali_normalization', 'bengali'), ('indic_normalization', 'hindi'),
                    ('german_normalization', 'german'), ('serbian_normalization', 'serbian'), ('kstem', 'english'), ('porter_stem', 'english')]:
        cases.append({'name': f'filter/{f}', 'request': {'tokenizer': 'standard', 'filter': ['lowercase', f], 'text': STEM_TEXTS[lang]}})
    for lang in STEM_TEXTS:
        cases.append({'name': f'analyzer/{lang}', 'request': {'analyzer': lang, 'text': STEM_TEXTS[lang]}})
    cases.append({'name': 'keyword/marked', 'request': {'tokenizer': 'standard', 'filter': ['lowercase', {'type': 'keyword_marker', 'keywords': ['running', 'ponies']}, {'type': 'stemmer', 'language': 'english'}], 'text': STEM_TEXTS['english'], 'explain': True}})
    cases.append({'name': 'keyword/repeat', 'request': {'tokenizer': 'standard', 'filter': ['lowercase', 'keyword_repeat', 'porter_stem', 'remove_duplicates'], 'text': STEM_TEXTS['english'], 'explain': True}})
    return cases


@group('regex')
def regex():
    cases = []
    texts = ['getHTTPResponseCode XMLHttpRequest2 version10Beta3 a1b2c3', 'foo@bar.com, bar@baz.org; alpha-beta_gamma', 'aaa bbb aaa ccc\nline2 end\r\nlast', 'Über straße ÉCOLE école 123-456-7890 (555) 123.4567', '']
    patterns = [
        ('camel', {'pattern': '([^\\p{L}\\d]+)|(?<=\\D)(?=\\d)|(?<=\\d)(?=\\D)|(?<=[\\p{L}&&[^\\p{Lu}]])(?=\\p{Lu})|(?<=\\p{Lu})(?=\\p{Lu}[\\p{L}&&[^\\p{Lu}]])'}),
        ('lookahead', {'pattern': '\\s+(?=[a-z])'}),
        ('neg_lookbehind', {'pattern': '(?<![a-z])\\d+'}),
        ('backref_group', {'pattern': '(\\w)\\1', 'group': 0}),
        ('named_group', {'pattern': '(?<user>\\w+)@(?<host>[\\w.]+)', 'group': 2}),
        ('possessive', {'pattern': '\\w++'}),
        ('atomic', {'pattern': '(?>\\w+)\\s'}),
        ('dollar', {'pattern': '\\w+$', 'group': 0}),
        ('multiline', {'pattern': '^\\w+', 'flags': 'MULTILINE', 'group': 0}),
        ('dotall', {'pattern': 'line2.*', 'flags': 'DOTALL', 'group': 0}),
        ('ci_unicode', {'pattern': 'école|straße', 'flags': 'CASE_INSENSITIVE|UNICODE_CASE', 'group': 0}),
        ('word_boundary', {'pattern': '\\b\\w{3}\\b', 'group': 0}),
        ('comments', {'pattern': '\\d{3} # area\n -? \\d{4}', 'flags': 'COMMENTS', 'group': 0}),
        ('lazy', {'pattern': '<.+?>', 'group': 0}),
        ('class_ops', {'pattern': '[\\w&&[^\\d_]]+', 'group': 0}),
        ('posix', {'pattern': '\\p{Punct}+'}),
        ('unicode_class', {'pattern': '\\w+', 'flags': 'UNICODE_CHARACTER_CLASS', 'group': 0}),
        ('empty_matches', {'pattern': 'x*'}),
    ]
    for pn, cfg in patterns:
        for i, t in enumerate(texts):
            cases.append({'name': f'tokenizer/{pn}/{i}', 'request': {'tokenizer': dict(cfg, type='pattern'), 'text': t}})
    reps = [
        ('groups', {'pattern': '(\\w+)@(\\w+)', 'replacement': '$2 at $1'}),
        ('named', {'pattern': '(?<a>\\d{3})-(?<b>\\d{3})', 'replacement': '${b}/${a}'}),
        ('escape', {'pattern': '\\d', 'replacement': '\\$'}),
        ('lookbehind', {'pattern': '(?<=\\d)-(?=\\d)', 'replacement': ''}),
        ('empty_match', {'pattern': 'z*', 'replacement': '_'}),
        ('missing_group', {'pattern': '(a)', 'replacement': '$3'}),
        ('first_only', {'pattern': '[aeiou]', 'replacement': '*', 'all': False}),
    ]
    for rn, cfg in reps:
        for i, t in enumerate(texts):
            cases.append({'name': f'filter/{rn}/{i}', 'request': {'tokenizer': 'keyword', 'filter': [dict(cfg, type='pattern_replace')], 'text': t}})
            if 'all' not in cfg:
                cases.append({'name': f'char_filter/{rn}/{i}', 'request': {'tokenizer': 'standard', 'char_filter': [dict(cfg, type='pattern_replace')], 'text': t}})
    caps = [
        ('email', {'patterns': ['(\\w+)@(\\w+)\\.(\\w+)']}),
        ('camel', {'patterns': ['([A-Z]?[a-z]+)', '(\\d+)'], 'preserve_original': False}),
        ('overlap', {'patterns': ['(a)(b)?', '(ab)'], 'preserve_original': True}),
    ]
    for cn, cfg in caps:
        for i, t in enumerate(texts):
            cases.append({'name': f'capture/{cn}/{i}', 'request': {'tokenizer': 'whitespace', 'filter': [dict(cfg, type='pattern_capture')], 'text': t}})
    bad = ['(?<=a+b', '[z-a]', '(?<n>x)(?<n>y)', 'a{2,1}', '\\', '*x', '(?<!\\w', '\\p{IsNope}']
    for i, b in enumerate(bad):
        cases.append({'name': f'error/{i}', 'request': {'tokenizer': {'type': 'pattern', 'pattern': b}, 'text': 'x'}})
    return cases


@group('graphs')
def graphs():
    cases = []
    syn = ['ny, new york, new york city', 'usa, united states, united states of america', 'wtf => what the fudge', 'dns => domain name system', 'ice cream, icecream']
    texts = ['I love new york city and the usa', 'wtf is dns in new york', 'icecream or ice cream', 'new york new york', 'the united states of america']
    for tn, t in enumerate(texts):
        for kind in ['synonym', 'synonym_graph']:
            cases.append({'name': f'{kind}/{tn}', 'request': {'tokenizer': 'standard', 'filter': ['lowercase', {'type': kind, 'synonyms': syn}], 'text': t}})
            cases.append({'name': f'{kind}/{tn}/explain', 'request': {'tokenizer': 'standard', 'filter': ['lowercase', {'type': kind, 'synonyms': syn}], 'text': t, 'explain': True}})
            cases.append({'name': f'{kind}/{tn}/noexpand', 'request': {'tokenizer': 'standard', 'filter': ['lowercase', {'type': kind, 'synonyms': syn, 'expand': False}], 'text': t}})
        cases.append({'name': f'flatten/{tn}', 'request': {'tokenizer': 'standard', 'filter': ['lowercase', {'type': 'synonym_graph', 'synonyms': syn}, 'flatten_graph'], 'text': t}})
        cases.append({'name': f'stop_syn/{tn}', 'request': {'tokenizer': 'standard', 'filter': ['lowercase', 'stop', {'type': 'synonym_graph', 'synonyms': ['love, adore', 'city => town']}], 'text': t}})
    wd = ["Wi-Fi PowerShot500 O'Neil's wi-fi-4000 SD-500X", "foo-bar_baz-2 x.y.z", "--leading trailing-- __x__"]
    for i, t in enumerate(wd):
        for kind in ['word_delimiter', 'word_delimiter_graph']:
            for cn, cfg in [('all', {'catenate_all': True, 'catenate_words': True, 'catenate_numbers': True, 'preserve_original': True}),
                            ('words', {'catenate_words': True}), ('plain', {})]:
                cases.append({'name': f'{kind}/{cn}/{i}', 'request': {'tokenizer': 'whitespace', 'filter': [dict(cfg, type=kind)], 'text': t, 'explain': True}})
        cases.append({'name': f'wdg_flatten/{i}', 'request': {'tokenizer': 'whitespace', 'filter': [{'type': 'word_delimiter_graph', 'catenate_all': True, 'preserve_original': True}, 'flatten_graph'], 'text': t}})
    shingle_texts = ['the quick brown fox jumps over the lazy dog', 'a b', 'single']
    for i, t in enumerate(shingle_texts):
        cases.append({'name': f'shingle_stop/{i}', 'request': {'tokenizer': 'standard', 'filter': ['stop', {'type': 'shingle', 'min_shingle_size': 2, 'max_shingle_size': 3}], 'text': t, 'explain': True}})
        cases.append({'name': f'shingle_filler/{i}', 'request': {'tokenizer': 'standard', 'filter': ['stop', {'type': 'shingle', 'filler_token': '', 'output_unigrams': False}], 'text': t}})
        cases.append({'name': f'common_grams_query/{i}', 'request': {'tokenizer': 'standard', 'filter': [{'type': 'common_grams', 'common_words': ['the', 'over', 'a'], 'query_mode': True}], 'text': t, 'explain': True}})
    cases.append({'name': 'multiplexer_synonyms', 'request': {'tokenizer': 'standard', 'filter': [{'type': 'multiplexer', 'filters': ['lowercase', 'uppercase, reverse']}], 'text': 'Quick Foxes', 'explain': True}})
    cases.append({'name': 'synonym_wordnet_multi', 'request': {'tokenizer': 'standard', 'filter': [{'type': 'synonym_graph', 'format': 'wordnet', 'synonyms': ["s(100000001,1,'new york',n,1,0).", "s(100000001,2,'big apple',n,1,0).", "s(100000002,1,'o''reilly',n,1,0).", "s(100000002,2,'publisher',n,1,0)."]}], 'text': 'big apple publisher', 'explain': True}})
    return cases


@group('index2')
def index2():
    cases = []
    settings_cases = [
        ('search_synonyms', {'analysis': {
            'filter': {'syn': {'type': 'synonym_graph', 'synonyms': ['laptop, notebook computer'], 'updateable': True}},
            'analyzer': {'idx': {'tokenizer': 'standard', 'filter': ['lowercase']},
                         'srch': {'tokenizer': 'standard', 'filter': ['lowercase', 'syn']}}}},
         [{'analyzer': 'srch', 'text': 'Cheap Laptop deals'}, {'analyzer': 'srch', 'text': 'notebook computer', 'explain': True}]),
        ('normalizer_chars', {'analysis': {
            'char_filter': {'m': {'type': 'mapping', 'mappings': ['ß => ss', '- => ']}, 'p': {'type': 'pattern_replace', 'pattern': '\\s+', 'replacement': ' '}},
            'normalizer': {'n': {'type': 'custom', 'char_filter': ['m', 'p'], 'filter': ['lowercase', 'asciifolding', 'trim']}}}},
         [{'normalizer': 'n', 'text': '  Straße-Été   ÜBER  '}, {'normalizer': 'n', 'text': ['A-B', 'Ç  d'], 'explain': True}]),
        ('gap', {'analysis': {'analyzer': {'g': {'tokenizer': 'whitespace', 'position_increment_gap': 7}}}},
         [{'analyzer': 'g', 'text': ['one two', 'three', 'four five']}, {'analyzer': 'g', 'text': ['one two', 'three'], 'explain': True}]),
        ('lang_options', {'analysis': {'analyzer': {
            'de': {'type': 'german', 'stem_exclusion': ['häuser']}, 'ru': {'type': 'russian', 'stopwords': '_none_'},
            'nl': {'type': 'dutch', 'stem_exclusion': ['fietsen']}, 'cjk2': {'type': 'cjk', 'stopwords': ['の']},
            'snow': {'type': 'snowball', 'language': 'Turkish'}, 'en2': {'type': 'english', 'stopwords_case': True, 'stopwords': ['The']}}}},
         [{'analyzer': a, 'text': t} for a, t in [('de', 'Die Häuser und Mädchen'), ('ru', 'и в на детьми'), ('nl', 'fietsen bromfietsen kinderen'),
                                                   ('cjk2', '日本語のテキスト'), ('snow', "İstanbul'da kitaplarımızdan"), ('en2', 'THE The the running')]]),
        ('condition_less', {'analysis': {'filter': {'mx': {'type': 'multiplexer', 'filters': ['lowercase', 'mx2']}, 'mx2': {'type': 'multiplexer', 'filters': ['uppercase']}},
                                         'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['mx']}}}},
         [{'analyzer': 'a', 'text': 'Mixed Case'}]),
        ('decompound', {'analysis': {'filter': {'dc': {'type': 'dictionary_decompounder', 'word_list': ['donau', 'dampf', 'schiff', 'fahrt', 'kapitän']}},
                                     'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['lowercase', 'dc']}}}},
         [{'analyzer': 'a', 'text': 'Donaudampfschifffahrtskapitän', 'explain': True}]),
    ]
    for name, settings, reqs in settings_cases:
        for i, r in enumerate(reqs):
            cases.append({'name': f'{name}/{i}', 'index': settings, 'request': r})
    failing = [
        ('synonym_after_wdg', {'analysis': {'filter': {'s': {'type': 'synonym', 'synonyms': ['a, b']}},
                                            'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['word_delimiter_graph', 's']}}}}),
        ('multiplexer_cycle', {'analysis': {'filter': {'m': {'type': 'multiplexer', 'filters': ['lowercase', 'm']}},
                                            'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['m']}}}}),
        ('stemmer_bad', {'analysis': {'filter': {'s': {'type': 'stemmer', 'language': 'nope'}}}}),
        ('snowball_bad_used', {'analysis': {'filter': {'s': {'type': 'snowball', 'language': 'Nope'}}, 'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['s']}}}}),
        ('snowball_bad_unused', {'analysis': {'filter': {'s': {'type': 'snowball', 'language': 'Nope'}}}}),
        ('mapping_bad_rule', {'analysis': {'char_filter': {'m': {'type': 'mapping', 'mappings': ['nope']}}}}),
        ('pattern_bad', {'analysis': {'tokenizer': {'t': {'type': 'pattern', 'pattern': '(unclosed'}}}}),
        ('normalizer_pattern_ok', {'analysis': {'normalizer': {'n': {'filter': ['lowercase', 'pattern_replace_x']}}, 'filter': {'pattern_replace_x': {'type': 'pattern_replace', 'pattern': 'a', 'replacement': 'b'}}}}),
        ('normalizer_bad_shingle', {'analysis': {'normalizer': {'n': {'filter': ['shingle']}}}}),
        ('condition_unknown_filter', {'analysis': {'filter': {'c': {'type': 'condition', 'filter': ['nope'], 'script': {'source': 'true'}}}, 'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['c']}}}}),
        ('word_delimiter_bad_table', {'analysis': {'filter': {'w': {'type': 'word_delimiter_graph', 'type_table': ['ab => ALPHA']}}}}),
        ('min_hash_too_many', {'analysis': {'filter': {'m': {'type': 'min_hash', 'hash_count': 10, 'bucket_count': 2000}}}}),
        ('hyphenation', {'analysis': {'filter': {'h': {'type': 'hyphenation_decompounder', 'word_list': ['a'], 'hyphenation_patterns_path': 'x.xml'}}}}),
        ('decompounder_no_list', {'analysis': {'filter': {'d': {'type': 'dictionary_decompounder'}}}}),
    ]
    for name, settings in failing:
        cases.append({'name': f'fail/{name}', 'index': settings, 'request': {'text': 'x'}})
    return cases


@group('snowball_vocab')
def snowball_vocab():
    """Every Snowball language on a sample of its official test vocabulary (snowball-data, BSD;
    directory in $SNOWBALL_VOCAB_DIR, one `<language>.txt` per language)."""
    import os
    d = os.environ['SNOWBALL_VOCAB_DIR']
    cases = []
    for f in sorted(os.listdir(d)):
        lang = f[:-4]
        words = [w.strip() for w in open(os.path.join(d, f), encoding='utf-8') if w.strip()]
        stride = max(1, len(words) // 200)
        sample = words[::stride][:200]
        name = lang.capitalize()
        cases.append({'name': name, 'request': {'tokenizer': 'whitespace', 'filter': [{'type': 'snowball', 'language': name}], 'text': ' '.join(sample)}})
    return cases


@group('regex_props')
def regex_props():
    """Java's Unicode property, block and script names in patterns, and their errors."""
    text = 'abc ÀÉ ΑΒ ἀἁ Ԁ Жж 中文 ひら カナ 한 ภาษา ١٢٣ א 𝐀 😀 ① ﬁ   x_1'
    names = ['InBasicLatin', 'InBASIC_LATIN', 'InBasic Latin', 'Inbasic latin', 'InBasic_Latin', 'InBasıcLatin',
             'InLatin-1 Supplement', 'InLatin-1Supplement', 'InLATIN_1_SUPPLEMENT', 'InLatin1Supplement',
             'InGreek', 'InGreekAndCoptic', 'InGreek and Coptic', 'InGREEK_EXTENDED', 'InCyrillicSupplement',
             'InCyrillic Supplementary', 'InCJKUnifiedIdeographs', 'InCJK_UNIFIED_IDEOGRAPHS', 'InHiragana',
             'InKatakana', 'InHangulSyllables', 'InThai', 'InArabic', 'InHebrew', 'InMathematicalAlphanumericSymbols',
             'InEmoticons', 'InEnclosedAlphanumerics', 'InAlphabeticPresentationForms', 'InSurrogatesArea',
             'InHighSurrogates', 'InNope', 'block=Greek', 'blk=greek', 'BLOCK=Thai', 'block=Nope', 'blk=',
             'IsLatin', 'IsLatn', 'IsGreek', 'IsHan', 'IsHani', 'IsHiragana', 'IsKatakana', 'IsHangul', 'IsThai',
             'IsArabic', 'IsCommon', 'IsInherited', 'IsZinh', 'IsUnknown', 'IsOld_Italic', 'IsOld Italic', 'IsNope',
             'script=Latin', 'sc=Grek', 'Script=Han', 'SC=thai', 'script=Nope', 'sc=', 'gc=Lu', 'gc=L', 'general_category=Nd',
             'gc=Alpha', 'gc=Nope', 'foo=bar', '=x', 'IsAlphabetic', 'IsALPHA', 'IsLu', 'IsL', 'IsPunct', 'IsWhite_Space',
             'IsEmoji', 'IsIdeographic', 'Lu', 'L', 'Nd', 'Alpha', 'Punct', 'javaLowerCase', 'IsJoin_Control', '']
    cases = []
    for n in names:
        cases.append({'name': f'p/{n}', 'request': {'tokenizer': {'type': 'pattern', 'pattern': '\\p{' + n + '}+', 'group': 0}, 'text': text}})
        cases.append({'name': f'P/{n}', 'request': {'tokenizer': {'type': 'pattern', 'pattern': '[\\P{' + n + '}&&[^ ]]+', 'group': 0}, 'text': text}})
    for flags in ['UNICODE_CHARACTER_CLASS', 'CASE_INSENSITIVE']:
        for n in ['gc=Alpha', 'Alpha', 'IsAlpha', 'Lower', 'gc=Lu', 'Lu']:
            cases.append({'name': f'flags/{flags}/{n}', 'request': {'tokenizer': {'type': 'pattern', 'pattern': '\\p{' + n + '}+', 'group': 0, 'flags': flags}, 'text': text}})
    # Java's Loop: an empty iteration ends a group repetition (even below its minimum count).
    loops = ['(?:\\b|a)+', '(?:\\b|a)*', '(?:\\b|a)+?', '(?:\\b|a){2,}', '(?:\\b|a){2}', '(\\b|a)+', '(?:a|\\b)+', '(?:\\B|a)+',
             '(?:\\b)+a', '(?:\\b|ab)+', '(?:a|(?=b))+b', '(?:(?=a)|a){3}', '(?:|a)+', '(?:x*|a)+', '(?:a*|b)+c', '((?:a|)+)b', '(?:a?b?)+x',
             '(?:\\b|a)*?b', '(a|\\b){2,3}', '(?:(a)|b|)+\\1']
    for i, pat in enumerate(loops):
        cases.append({'name': f'loops/{i}', 'request': {'tokenizer': {'type': 'pattern', 'pattern': pat, 'group': 0}, 'text': 'aaa aab abcab xbx'}})
        cases.append({'name': f'loops/{i}/split', 'request': {'tokenizer': {'type': 'pattern', 'pattern': pat}, 'text': 'aaa aab abcab xbx'}})
    for pn, pat in [('grapheme', '\\X'), ('named', '\\N{LATIN SMALL LETTER A}'), ('gb', '\\b{g}'), ('linebreak', '\\R'), ('h', '\\h+'), ('v', '\\v+')]:
        cases.append({'name': f'other/{pn}', 'request': {'tokenizer': {'type': 'pattern', 'pattern': pat, 'group': 0}, 'text': 'éx 👨‍👩‍👧 a\r\nb   \t'}})
    return cases


def fuzz_regex(rng, depth=0):
    """A random Java regular expression from the constructs analysis users write."""
    atoms = ['a', 'b', 'c', 'x', '1', '\\.', '-', ' ', '\\w', '\\W', '\\d', '\\D', '\\s', '\\S', '.', '[a-c]', '[^ab]', '[\\w&&[^\\d]]',
             '[a-z0-9_]', '\\p{L}', '\\p{Lu}', '\\p{IsAlphabetic}', '[A-Z]', 'é', 'ß', 'İ', '\\u00e9', '\\x41', '\\t', '[.]', '\\b', '\\B', '^', '$']
    r = rng.random()
    if depth > 2 or r < 0.35:
        return rng.choice(atoms)
    if r < 0.55:
        return ''.join(fuzz_regex(rng, depth + 1) for _ in range(rng.randint(2, 4)))
    if r < 0.65:
        return '(' + '|'.join(fuzz_regex(rng, depth + 1) for _ in range(rng.randint(2, 3))) + ')'
    if r < 0.85:
        q = rng.choice(['*', '+', '?', '{2}', '{1,3}', '{0,2}', '*?', '+?', '??', '*+', '++', '{2,}'])
        return '(?:' + fuzz_regex(rng, depth + 1) + ')' + q
    if r < 0.9:
        return rng.choice(['(?=', '(?!', '(?<=', '(?<!']) + rng.choice(atoms) + ')'
    if r < 0.95:
        return '(' + fuzz_regex(rng, depth + 1) + ')\\1'
    return '(?>' + fuzz_regex(rng, depth + 1) + ')'


@group('fuzz2')
def fuzz2():
    """Seeded random configurations: Java regexes, synonym graphs, word delimiters, shingles."""
    import random
    rng = random.Random(4242)
    cases = []
    regex_text = ['abc xab ABC a1b2 c.c-x  ß é É İ i', 'xxabcab\nabc\r\nb', 'aaa bbb ccc', 'Hello World 123 _x_ a-b-c', '']
    for i in range(150):
        pat = fuzz_regex(rng)
        flags = rng.choice(['', '', 'CASE_INSENSITIVE', 'CASE_INSENSITIVE|UNICODE_CASE', 'MULTILINE', 'DOTALL', 'UNICODE_CHARACTER_CLASS', 'COMMENTS'])
        group = rng.choice([-1, 0, 0, 1])
        text = rng.choice(regex_text)
        tk = {'type': 'pattern', 'pattern': pat, 'group': group}
        if flags:
            tk['flags'] = flags
        cases.append({'name': f'regex/{i}', 'request': {'tokenizer': tk, 'text': text}})
        if i % 3 == 0:
            cases.append({'name': f'regex_replace/{i}', 'request': {'tokenizer': 'keyword', 'filter': [{'type': 'pattern_replace', 'pattern': pat, 'replacement': rng.choice(['_', '<$0>', '', '$1'])}], 'text': text}})
    vocab = ['a', 'b', 'c', 'd', 'e']
    for i in range(120):
        rules = []
        for _ in range(rng.randint(1, 4)):
            lhs = [' '.join(rng.choice(vocab) for _ in range(rng.randint(1, 3))) for _ in range(rng.randint(1, 3))]
            if rng.random() < 0.4:
                rhs = [' '.join(rng.choice(vocab) for _ in range(rng.randint(1, 3))) for _ in range(rng.randint(1, 2))]
                rules.append(', '.join(lhs) + ' => ' + ', '.join(rhs))
            else:
                rules.append(', '.join(lhs))
        text = ' '.join(rng.choice(vocab + ['x']) for _ in range(rng.randint(1, 8)))
        kind = rng.choice(['synonym', 'synonym_graph', 'synonym_graph'])
        expand = rng.random() < 0.8
        filters = [{'type': kind, 'synonyms': rules, 'expand': expand}]
        if kind == 'synonym_graph' and rng.random() < 0.3:
            filters.append('flatten_graph')
        if rng.random() < 0.2:
            filters.insert(0, {'type': 'stop', 'stopwords': ['x']})
        cases.append({'name': f'syn/{i}', 'request': {'tokenizer': 'whitespace', 'filter': filters, 'text': text, 'explain': i % 4 == 0}})
    wd_words = ['Wi-Fi', 'PowerShot500', "O'Neil's", 'SD-500x', 'foo_bar', 'a1b2', '--x--', '3.14', 'ÉCOLE-école', 'x86_64', 'C++', 'AT&T', "can't", 'IBM.com']
    opts = ['generate_word_parts', 'generate_number_parts', 'catenate_words', 'catenate_numbers', 'catenate_all', 'split_on_case_change',
            'preserve_original', 'split_on_numerics', 'stem_english_possessive']
    for i in range(80):
        cfg = {'type': rng.choice(['word_delimiter', 'word_delimiter_graph'])}
        for o in opts:
            if rng.random() < 0.5:
                cfg[o] = rng.random() < 0.5
        if cfg['type'] == 'word_delimiter_graph' and rng.random() < 0.3:
            cfg['adjust_offsets'] = False
        text = ' '.join(rng.choice(wd_words) for _ in range(rng.randint(1, 4)))
        filters = [cfg]
        if rng.random() < 0.3:
            filters.append('flatten_graph')
        cases.append({'name': f'wd/{i}', 'request': {'tokenizer': 'whitespace', 'filter': filters, 'text': text}})
    for i in range(60):
        mn = rng.randint(2, 3)
        cfg = {'type': 'shingle', 'min_shingle_size': mn, 'max_shingle_size': rng.randint(mn, mn + 1),
               'output_unigrams': rng.random() < 0.6, 'output_unigrams_if_no_shingles': rng.random() < 0.5}
        if rng.random() < 0.3:
            cfg['token_separator'] = rng.choice(['_', '', '  '])
        if rng.random() < 0.3:
            cfg['filler_token'] = rng.choice(['', 'F', '_'])
        text = ' '.join(rng.choice(['the', 'a', 'quick', 'fox', 'of', 'jumps', 'x']) for _ in range(rng.randint(1, 7)))
        filters = ([{'type': 'stop', 'stopwords': ['the', 'a', 'of']}] if rng.random() < 0.5 else []) + [cfg]
        cases.append({'name': f'shingle/{i}', 'request': {'tokenizer': 'standard', 'filter': filters, 'text': text}})
    for i in range(40):
        cfg = {'type': 'common_grams', 'common_words': ['the', 'a', 'of'], 'query_mode': rng.random() < 0.5, 'ignore_case': rng.random() < 0.5}
        text = ' '.join(rng.choice(['the', 'The', 'a', 'quick', 'fox', 'of', 'OF']) for _ in range(rng.randint(1, 6)))
        cases.append({'name': f'common_grams/{i}', 'request': {'tokenizer': 'whitespace', 'filter': [cfg], 'text': text}})
    for i in range(40):
        rules = []
        for _ in range(rng.randint(1, 4)):
            a = ''.join(rng.choice('abcé ') for _ in range(rng.randint(1, 3))).replace(' ', '\\u0020')
            b = ''.join(rng.choice('xyz') for _ in range(rng.randint(0, 3)))
            rules.append(f'{a} => {b}')
        text = ''.join(rng.choice('abcé xy') for _ in range(rng.randint(0, 20)))
        cases.append({'name': f'mapping/{i}', 'request': {'tokenizer': rng.choice(['standard', 'keyword']), 'char_filter': [{'type': 'mapping', 'mappings': rules}], 'text': text}})
    for i in range(40):
        cfg = {'type': 'path_hierarchy', 'delimiter': rng.choice(['/', '-', '.']), 'skip': rng.randint(0, 2), 'reverse': rng.random() < 0.5}
        if rng.random() < 0.4:
            cfg['replacement'] = rng.choice(['|', '/', '_'])
        text = ''.join(rng.choice(['/', '-', '.', 'a', 'bc', 'd']) for _ in range(rng.randint(0, 12)))
        cases.append({'name': f'path/{i}', 'request': {'tokenizer': cfg, 'text': text}})
    for i in range(40):
        mn = rng.randint(1, 3)
        cfg = {'type': rng.choice(['ngram', 'edge_ngram']), 'min_gram': mn, 'max_gram': mn + rng.randint(0, 1),
               'token_chars': rng.sample(['letter', 'digit', 'whitespace', 'punctuation', 'symbol'], rng.randint(0, 3))}
        text = fuzz_text(rng, ['ascii', 'latin', 'emoji'], rng.randint(1, 10))
        cases.append({'name': f'ngram_tk/{i}', 'request': {'tokenizer': cfg, 'text': text}})
    langs = ['arabic', 'armenian', 'basque', 'bengali', 'brazilian', 'bulgarian', 'catalan', 'czech', 'danish', 'dutch', 'english', 'estonian', 'finnish',
             'french', 'galician', 'german', 'greek', 'hindi', 'hungarian', 'indonesian', 'irish', 'italian', 'latvian', 'lithuanian', 'norwegian',
             'persian', 'portuguese', 'romanian', 'russian', 'serbian', 'sorani', 'spanish', 'swedish', 'turkish']
    for lang in langs:
        for j in range(3):
            text = fuzz_text(rng, ['ascii', 'latin', 'latin', 'scripts', 'numbers'], rng.randint(5, 25))
            cases.append({'name': f'lang/{lang}/{j}', 'request': {'analyzer': lang, 'text': text}})
    return cases


@group('fuzz_regex')
def fuzz_regex_group():
    """More seeded random Java regexes (pattern tokenizer and pattern_replace), and
    case-insensitive matching around the characters whose case mappings are irregular."""
    import random
    rng = random.Random(777)
    texts = ['abc xab ABC a1b2 c.c-x  ß é É İ i', 'xxabcab\nabc\r\nb', 'aaa bbb ccc', 'Hello World 123 _x_ a-b-c', '', 'aaa aab abcab xbx',
             'ab́c ΣΑΣ σας ﬁ']
    cases = []
    for i in range(600):
        pat = fuzz_regex(rng)
        flags = rng.choice(['', '', 'CASE_INSENSITIVE', 'CASE_INSENSITIVE|UNICODE_CASE', 'MULTILINE', 'DOTALL', 'UNICODE_CHARACTER_CLASS',
                            'COMMENTS', 'MULTILINE|DOTALL'])
        tk = {'type': 'pattern', 'pattern': pat, 'group': rng.choice([-1, 0, 0, 1, 2])}
        if flags:
            tk['flags'] = flags
        req = {'tokenizer': tk, 'text': rng.choice(texts)}
        if rng.random() < 0.3:
            req = {'tokenizer': 'keyword', 'filter': [{'type': 'pattern_replace', 'pattern': pat,
                                                       'replacement': rng.choice(['_', '<$0>', '', '$1', '${x}', '\\$'])}], 'text': rng.choice(texts)}
        cases.append({'name': f'regex/{i}', 'request': req})
    atoms = ['k', 's', 'ß', 'i', 'I', 'µ', 'ÿ', 'σ', 'Σ', 'ω', 'å', '[k-s]', '[a-zA-Z]', '[à-ÿ]', '[α-ω]', '[A-Z]', '[\\u0100-\\u017f]', '[^a-z]',
             '[ks]', '[ßµÿ]', '[Ii]', '\\w', '[\\w&&[^k]]', '[ſK]', '\\x{1E9E}', '[ı-ŀ]', '[Ω]', '[ά-ώ]', '[\\u2100-\\u2200]', '\\p{Lu}', '[Ä-Ö]']
    text = 'kKKsSſßẞiIıİµΜμÿŸσΣςωΩΩåÅÅǅǄǆ θϑ ﬁ abc ÀÉ'
    for i in range(300):
        pat = ''.join(rng.choice(atoms) for _ in range(rng.randint(1, 2)))
        if rng.random() < 0.5:
            pat = '(?:' + pat + ')+'
        flags = rng.choice(['CASE_INSENSITIVE', 'CASE_INSENSITIVE|UNICODE_CASE', 'UNICODE_CASE', 'CASE_INSENSITIVE|UNICODE_CHARACTER_CLASS', ''])
        cases.append({'name': f'ci/{i}', 'request': {'tokenizer': {'type': 'pattern', 'pattern': pat, 'group': 0, 'flags': flags}, 'text': text}})
    return cases


def fuzz_lucene_regexp(rng, depth=0):
    """A random Lucene `RegExp` (the syntax of `simple_pattern` and `simple_pattern_split`)."""
    atoms = ['a', 'b', 'c', '1', 'é', ' ', '\\.', '\\-', '\\\\', '\\s', '\\w', '\\W', '\\d', '[a-c]', '[^a]', '[a-zé]', '[0-9]',
             '"ab"', '<1-12>', '<01-10>', '@', '#', '~', '.']
    r = rng.random()
    if depth > 2 or r < 0.35:
        return rng.choice(atoms)
    if r < 0.55:
        return ''.join(fuzz_lucene_regexp(rng, depth + 1) for _ in range(rng.randint(2, 3)))
    if r < 0.7:
        return '(' + '|'.join(fuzz_lucene_regexp(rng, depth + 1) for _ in range(rng.randint(2, 3))) + ')'
    if r < 0.8:
        return '(' + fuzz_lucene_regexp(rng, depth + 1) + ')&(' + fuzz_lucene_regexp(rng, depth + 1) + ')'
    if r < 0.95:
        return '(' + fuzz_lucene_regexp(rng, depth + 1) + ')' + rng.choice(['*', '+', '?', '{2}', '{1,3}', '{2,}', '{0,2}'])
    return '~(' + fuzz_lucene_regexp(rng, depth + 1) + ')'


@group('lucene_regexp')
def lucene_regexp():
    """Seeded random Lucene regular expressions through `simple_pattern` (which rejects automata
    Lucene's construction leaves nondeterministic) and `simple_pattern_split` (which determinizes)."""
    import random
    rng = random.Random(1010)
    texts = ['aaaa', 'abc aab 123 a.b-c 07 10 12 é éa', 'cba abcabc 1 2 3', '', 'a\\b \\\\ 1-2-3 ~@#']
    cases = []
    for i in range(800):
        ty = rng.choice(['simple_pattern', 'simple_pattern_split'])
        cases.append({'name': f'lre/{i}', 'request': {'tokenizer': {'type': ty, 'pattern': fuzz_lucene_regexp(rng)},
                                                      'text': rng.choice(texts)}})
    for i, pat in enumerate(['[a', 'a{3,2}', '<abc>', '(a', 'a)', '"ab', '[z-a]', '<10-1>', '\\', 'a{', '{2}', '*a', 'a**', '()', '(a&b){2}', 'a|']):
        for ty in ['simple_pattern', 'simple_pattern_split']:
            cases.append({'name': f'syntax/{i}/{ty}', 'request': {'tokenizer': {'type': ty, 'pattern': pat}, 'text': 'aa ab'}})
    return cases


REGISTRY_NAMES = sorted(set("""
apostrophe arabic arabic_normalization arabic_stem armenian asciifolding basque bengali bengali_normalization brazilian
brazilian_stem bulgarian catalan char_group chinese cjk cjk_bigram cjk_width classic common_grams condition czech
czech_stem danish decimal_digit delimited_payload delimited_payload_filter dictionary_decompounder dutch dutch_stem
edge_ngram edgeNGram elision english English estonian fingerprint finnish flatten_graph french french_stem galician
german german_normalization german_stem greek hindi hindi_normalization html_strip htmlStrip hungarian
hyphenation_decompounder indic_normalization indonesian irish italian keep keep_types keyword keyword_marker
keyword_repeat kstem latvian length letter limit lithuanian lowercase mapping min_hash multiplexer ngram nGram
norwegian path_hierarchy PathHierarchy pattern pattern_capture pattern_replace persian persian_normalization
persian_stem porter_stem portuguese predicate_token_filter remove_duplicates reverse romanian russian Russian
russian_stem scandinavian_folding scandinavian_normalization serbian serbian_normalization shingle simple
simple_pattern simple_pattern_split snowball sorani sorani_normalization spanish stemmer stemmer_override stop
swedish synonym synonym_graph thai trim truncate turkish type_as_payload uax_url_email unique uppercase whitespace
word_delimiter word_delimiter_graph standard default hunspell _na_ custom greek_lowercase irish_lowercase
turkish_lowercase porterStem uaxUrlEmail edgeNGram_tokenizer stemmer_override keyword_tokenizer cjk_width
""".split()))
REGISTRY_TEXT = "The QUICK brown-fox's wi-fi 123 <b>x</b> &amp; Ünïcödé façade 我是中国人 ny l'avion a|1.5 running ภาษาไทย"


@group('registry')
def registry():
    """Every component name Elasticsearch registers (and some it does not) in every role:
    pre-configured tokenizer, token filter, char filter, analyzer, in a normalizer-style
    request, and as the `type` of an inline definition with default settings."""
    cases = []
    t = REGISTRY_TEXT
    for n in REGISTRY_NAMES:
        cases.append({'name': f'tokenizer/{n}', 'request': {'tokenizer': n, 'text': t}})
        cases.append({'name': f'filter/{n}', 'request': {'tokenizer': 'whitespace', 'filter': [n], 'text': t}})
        cases.append({'name': f'char_filter/{n}', 'request': {'tokenizer': 'keyword', 'char_filter': [n], 'text': t}})
        cases.append({'name': f'analyzer/{n}', 'request': {'analyzer': n, 'text': t}})
        cases.append({'name': f'normalizer_filter/{n}', 'request': {'filter': [n], 'text': t}})
        cases.append({'name': f'normalizer_char_filter/{n}', 'request': {'char_filter': [n], 'text': t}})
        cases.append({'name': f'tokenizer_type/{n}', 'request': {'tokenizer': {'type': n}, 'text': t}})
        cases.append({'name': f'filter_type/{n}', 'request': {'tokenizer': 'whitespace', 'filter': [{'type': n}], 'text': t}})
        cases.append({'name': f'char_filter_type/{n}', 'request': {'tokenizer': 'keyword', 'char_filter': [{'type': n}], 'text': t}})
        cases.append({'name': f'explain/{n}', 'request': {'tokenizer': 'whitespace', 'filter': [n], 'text': ['Ab cD', 'x'], 'explain': True}})
    return cases


@group('registry_index')
def registry_index():
    """Every component type defined in index settings with default settings, unreferenced and
    referenced by an analyzer (and the same for analyzer and normalizer types)."""
    cases = []
    for n in REGISTRY_NAMES:
        for kind, ref in [('filter', 'filter'), ('tokenizer', 'tokenizer'), ('char_filter', 'char_filter')]:
            cases.append({'name': f'{kind}/{n}/unused', 'index': {'analysis': {kind: {'c': {'type': n}}}},
                          'request': {'text': 'x'}})
            an = {'tokenizer': 'whitespace'}
            an[ref] = 'c' if kind == 'tokenizer' else ['c']
            cases.append({'name': f'{kind}/{n}/used', 'index': {'analysis': {kind: {'c': {'type': n}}, 'analyzer': {'a': an}}},
                          'request': {'analyzer': 'a', 'text': REGISTRY_TEXT}})
        cases.append({'name': f'analyzer/{n}', 'index': {'analysis': {'analyzer': {'a': {'type': n}}}},
                      'request': {'analyzer': 'a', 'text': REGISTRY_TEXT}})
        cases.append({'name': f'normalizer/{n}', 'index': {'analysis': {'normalizer': {'n': {'type': 'custom', 'filter': [n]}}}},
                      'request': {'normalizer': 'n', 'text': REGISTRY_TEXT}})
        cases.append({'name': f'normalizer_cf/{n}', 'index': {'analysis': {'normalizer': {'n': {'type': 'custom', 'char_filter': [n]}}}},
                      'request': {'normalizer': 'n', 'text': REGISTRY_TEXT}})
        cases.append({'name': f'normalizer_type/{n}', 'index': {'analysis': {'normalizer': {'n': {'type': n}}}},
                      'request': {'normalizer': 'n', 'text': REGISTRY_TEXT}})
    return cases


@group('stream_end')
def stream_end():
    """Where each tokenizer reports its final offset when a `limit` filter stops reading it early
    (the next value of a multi-valued text starts after that offset)."""
    tokenizers = ['standard', 'whitespace', 'letter', 'lowercase', 'classic', 'uax_url_email', 'keyword', 'thai',
                  {'type': 'char_group', 'tokenize_on_chars': ['whitespace', '-']}, {'type': 'pattern', 'pattern': '\\W+'},
                  {'type': 'pattern', 'pattern': '\\w+', 'group': 0}, {'type': 'simple_pattern', 'pattern': '[a-z]+'},
                  {'type': 'simple_pattern_split', 'pattern': '[ ,.]+'}, {'type': 'ngram', 'min_gram': 1, 'max_gram': 2},
                  {'type': 'edge_ngram', 'min_gram': 1, 'max_gram': 2, 'token_chars': ['letter']}, 'path_hierarchy',
                  {'type': 'path_hierarchy', 'reverse': True}, {'type': 'standard', 'max_token_length': 2}]
    texts = [['ab cd, ef. gh!! ', 'xy z'], ['/ab/cd/ef/', 'x/y'], ['ab  ', 'cd'], ['ภาษาไทย ง่าย นิด', 'x y']]
    cases = []
    for i, tk in enumerate(tokenizers):
        for j, text in enumerate(texts):
            for k, lim in enumerate([{'type': 'limit', 'max_token_count': 1}, {'type': 'limit', 'max_token_count': 2},
                                     {'type': 'limit', 'max_token_count': 1, 'consume_all_tokens': True}]):
                cases.append({'name': f'{i}/{j}/{k}', 'request': {'tokenizer': tk, 'filter': [lim], 'text': text}})
            if tk == 'thai':
                # Several 1024-unit chunks: the final offset is the end of the chunk being read.
                long_text = ('ภาษาไทยง่ายนิดเดียว abc ' * 40 + '\n') * 4
                for lim in [1, 2, 60, 200]:
                    cases.append({'name': f'{i}/{j}/thai_long_{lim}', 'request': {'tokenizer': tk, 'filter': [{'type': 'limit', 'max_token_count': lim}],
                                                                                    'text': [long_text, 'x']}})
            cases.append({'name': f'{i}/{j}/html', 'request': {'tokenizer': tk, 'char_filter': ['html_strip'],
                                                               'filter': [{'type': 'limit', 'max_token_count': 1}],
                                                               'text': ['<b>a</b>b <i>cd</i> ef', 'x']}})
    return cases


PARAMS = {
    'tokenizer': {
        'standard': ['max_token_length'], 'whitespace': ['max_token_length'], 'letter': ['max_token_length'],
        'classic': ['max_token_length'], 'uax_url_email': ['max_token_length'], 'keyword': ['buffer_size'],
        'lowercase': ['max_token_length'],
        'ngram': ['min_gram', 'max_gram', 'token_chars', 'custom_token_chars'],
        'edge_ngram': ['min_gram', 'max_gram', 'token_chars', 'custom_token_chars'],
        'pattern': ['pattern', 'flags', 'group'], 'simple_pattern': ['pattern'], 'simple_pattern_split': ['pattern'],
        'char_group': ['tokenize_on_chars', 'max_token_length'],
        'path_hierarchy': ['delimiter', 'replacement', 'buffer_size', 'reverse', 'skip'],
    },
    'char_filter': {
        'html_strip': ['escaped_tags'], 'mapping': ['mappings'], 'pattern_replace': ['pattern', 'replacement', 'flags'],
    },
    'filter': {
        'asciifolding': ['preserve_original'], 'cjk_bigram': ['ignored_scripts', 'output_unigrams'],
        'common_grams': ['common_words', 'ignore_case', 'query_mode'], 'delimited_payload': ['delimiter', 'encoding'],
        'dictionary_decompounder': ['word_list', 'min_word_size', 'min_subword_size', 'max_subword_size', 'only_longest_match'],
        'edge_ngram': ['min_gram', 'max_gram', 'preserve_original', 'side'], 'elision': ['articles', 'articles_case'],
        'fingerprint': ['max_output_size', 'separator'], 'keep_types': ['types', 'mode'], 'keep': ['keep_words', 'keep_words_case'],
        'keyword_marker': ['keywords', 'keywords_pattern', 'ignore_case'], 'length': ['min', 'max'],
        'limit': ['max_token_count', 'consume_all_tokens'], 'lowercase': ['language'], 'uppercase': ['language'],
        'min_hash': ['hash_count', 'bucket_count', 'hash_set_size', 'with_rotation'],
        'multiplexer': ['filters', 'preserve_original'], 'ngram': ['min_gram', 'max_gram', 'preserve_original'],
        'pattern_capture': ['patterns', 'preserve_original'], 'pattern_replace': ['pattern', 'replacement', 'all'],
        'shingle': ['max_shingle_size', 'min_shingle_size', 'output_unigrams', 'output_unigrams_if_no_shingles', 'token_separator', 'filler_token'],
        'snowball': ['language'], 'stemmer': ['language', 'name'], 'stemmer_override': ['rules'],
        'stop': ['stopwords', 'ignore_case', 'remove_trailing'],
        'synonym': ['synonyms', 'expand', 'lenient', 'format', 'updateable', 'synonyms_set'],
        'synonym_graph': ['synonyms', 'expand', 'lenient', 'format', 'updateable', 'synonyms_set'],
        'truncate': ['length'], 'unique': ['only_on_same_position'],
        'word_delimiter': ['generate_word_parts', 'generate_number_parts', 'catenate_words', 'catenate_numbers', 'catenate_all',
                           'split_on_case_change', 'preserve_original', 'split_on_numerics', 'stem_english_possessive',
                           'protected_words', 'type_table'],
        'word_delimiter_graph': ['generate_word_parts', 'generate_number_parts', 'catenate_words', 'catenate_numbers', 'catenate_all',
                                 'split_on_case_change', 'preserve_original', 'split_on_numerics', 'stem_english_possessive',
                                 'protected_words', 'type_table', 'adjust_offsets', 'ignore_keywords'],
        'condition': ['filter'], 'remove_duplicates': [], 'trim': [], 'reverse': [], 'kstem': [], 'porter_stem': [],
    },
    'analyzer': {
        'standard': ['max_token_length', 'stopwords'], 'stop': ['stopwords'], 'pattern': ['pattern', 'flags', 'lowercase', 'stopwords'],
        'fingerprint': ['separator', 'max_output_size', 'stopwords'], 'english': ['stopwords', 'stem_exclusion'],
        'custom': ['position_increment_gap', 'offset_gap'], 'snowball': ['language', 'stopwords'],
    },
}
# Required settings so that the parameter under test is what fails.
PARAM_BASE = {
    ('filter', 'synonym'): {'synonyms': ['a, b']}, ('filter', 'synonym_graph'): {'synonyms': ['a, b']},
    ('filter', 'keep_types'): {'types': ['<ALPHANUM>']}, ('filter', 'keep'): {'keep_words': ['a']},
    ('filter', 'stemmer_override'): {'rules': ['a => b']}, ('filter', 'elision'): {'articles': ['l']},
    ('filter', 'pattern_capture'): {'patterns': ['(a)']}, ('filter', 'pattern_replace'): {'pattern': 'a'},
    ('filter', 'dictionary_decompounder'): {'word_list': ['a']}, ('filter', 'common_grams'): {'common_words': ['a']},
    ('filter', 'multiplexer'): {'filters': ['lowercase']}, ('filter', 'condition'): {'filter': ['lowercase'], 'script': {'source': 'true'}},
    ('filter', 'truncate'): {'length': 3}, ('char_filter', 'mapping'): {'mappings': ['a => b']},
    ('char_filter', 'pattern_replace'): {'pattern': 'a'}, ('filter', 'keyword_marker'): {'keywords': ['a']},
    ('analyzer', 'custom'): {'tokenizer': 'standard'}, ('tokenizer', 'ngram'): {}, ('tokenizer', 'edge_ngram'): {},
}
PARAM_VALUES = [('str', 'abc'), ('neg', -1), ('zero', 0), ('float', 1.5), ('floatstr', '1.5'), ('bool', True), ('boolstr', 'false'),
                ('list', [1, 2]), ('obj', {'x': 1}), ('null', None), ('empty', ''), ('big', 99999999999), ('spaced', ' 5 '),
                ('one', 1), ('emptylist', []), ('mixedlist', ['a', 1]), ('upper', 'TRUE'), ('five', 5), ('comma', 'a, b,c'),
                ('blank', '  '), ('hex', '0x10'), ('plus', '+3'), ('unicode_digits', '٣')]
PARAM_TEXT = 'Abc-DEF a1b2 The l\'avion 1.5 ab|1'


@group('params')
def params():
    """Every parameter of every configurable component with ill-typed and edge values, through
    `_analyze` inline definitions: what is accepted, how it is coerced, and the exact error."""
    cases = []
    for kind, comps in PARAMS.items():
        if kind == 'analyzer':
            continue
        for comp, names in comps.items():
            for pn in names:
                for vn, v in PARAM_VALUES:
                    d = dict(PARAM_BASE.get((kind, comp), {}))
                    d['type'] = comp
                    d[pn] = v
                    req = {'text': PARAM_TEXT}
                    if kind == 'tokenizer':
                        req['tokenizer'] = d
                    elif kind == 'filter':
                        req['tokenizer'] = 'standard'
                        req['filter'] = [d]
                    else:
                        req['tokenizer'] = 'standard'
                        req['char_filter'] = [d]
                    cases.append({'name': f'{kind}/{comp}/{pn}/{vn}', 'compact': True, 'request': req})
    return cases


@group('params_index')
def params_index():
    """The parameters of analyzers (and a sample of other components) in index settings, where
    settings are flattened and validated when the index is created."""
    cases = []
    for comp, names in PARAMS['analyzer'].items():
        for pn in names:
            for vn, v in PARAM_VALUES:
                d = dict(PARAM_BASE.get(('analyzer', comp), {}))
                d['type'] = comp
                d[pn] = v
                cases.append({'name': f'analyzer/{comp}/{pn}/{vn}', 'index': {'analysis': {'analyzer': {'a': d}}},
                              'request': {'analyzer': 'a', 'text': ['Abc the foo', 'x']}})
    for comp, pn in [('stop', 'stopwords'), ('shingle', 'max_shingle_size'), ('ngram', 'max_gram'), ('word_delimiter_graph', 'type_table'),
                     ('length', 'max'), ('synonym', 'synonyms'), ('stemmer', 'language'), ('limit', 'max_token_count')]:
        for vn, v in PARAM_VALUES:
            d = dict(PARAM_BASE.get(('filter', comp), {}))
            d['type'] = comp
            d[pn] = v
            cases.append({'name': f'filter/{comp}/{pn}/{vn}', 'index': {'analysis': {'filter': {'f': d},
                                                                                     'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['f']}}}},
                          'request': {'analyzer': 'a', 'text': PARAM_TEXT}})
    return cases


@group('long')
def long_texts():
    """Texts longer than the readers' buffers (CharTokenizer's 4096 chars, the JFlex scanners'
    buffers, the thai tokenizer's 1024-char chunks, html_strip's read-ahead), with surrogate
    pairs and long tokens placed across the boundaries. Responses are stored compactly
    (`"compact": [[token, start, end, type, position(, positionLength)], ...]`)."""
    import random
    rng = random.Random(4096)
    word_pools = ['ascii', 'latin', 'scripts', 'cjk', 'sea', 'emoji', 'numbers']
    crafted = [
        ('a4095_emoji', 'a' * 4095 + '😀' + 'b c'),
        ('sp4095_emoji', ' ' * 4095 + '😀x yz'),
        ('x2047_emoji', 'x ' * 2047 + 'ab😀cd ef'),
        ('word5000', 'w' * 5000 + ' tail'),
        ('url5000', 'see http://example.com/' + 'p' * 5000 + ' and mail@' + 'd' * 300 + '.com ok'),
        ('emoji_run', '😀' * 3000 + ' x'),
        ('cjk_run', '中文' * 2600 + ' x'),
        ('thai_run', 'ภาษาไทย' * 700 + ' x'),
        ('tags', '<b>' * 1500 + 'x' + '</b>' * 1500 + ' <!-- ' + 'c' * 5000 + ' --> y'),
        ('long_tag', '<a href="' + 'h' * 9000 + '">x</a> y'),
        ('entity', '&amp;' * 2000 + ' &#x263a;' * 500),
        ('path', '/' + '/'.join('seg%d' % i for i in range(900))),
    ]
    plans = [
        ('standard', {'tokenizer': 'standard'}), ('standard4', {'tokenizer': {'type': 'standard', 'max_token_length': 4}}),
        ('standard300', {'tokenizer': {'type': 'standard', 'max_token_length': 300}}),
        ('classic', {'tokenizer': 'classic'}), ('uax', {'tokenizer': 'uax_url_email'}), ('whitespace', {'tokenizer': 'whitespace'}),
        ('letter', {'tokenizer': 'letter'}), ('lowercase', {'tokenizer': 'lowercase'}),
        ('char_group', {'tokenizer': {'type': 'char_group', 'tokenize_on_chars': ['whitespace', 'punctuation']}}),
        ('keyword', {'tokenizer': 'keyword'}), ('pattern', {'tokenizer': 'pattern'}),
        ('simple_pattern', {'tokenizer': {'type': 'simple_pattern', 'pattern': '[a-zA-Z0-9]+'}}),
        ('thai', {'tokenizer': 'thai'}), ('path', {'tokenizer': 'path_hierarchy'}),
        ('path_rev', {'tokenizer': {'type': 'path_hierarchy', 'reverse': True, 'skip': 2}}),
        ('html_kw', {'tokenizer': 'keyword', 'char_filter': ['html_strip']}),
        ('html_std', {'tokenizer': 'standard', 'char_filter': ['html_strip']}),
        ('mapping', {'tokenizer': 'whitespace', 'char_filter': [{'type': 'mapping', 'mappings': ['a => xyz', 'bc => ', '😀 => e']}]}),
        ('pattern_replace', {'tokenizer': 'whitespace', 'char_filter': [{'type': 'pattern_replace', 'pattern': '(\\w)\\w*', 'replacement': '$1'}]}),
        ('std_an', {'analyzer': 'standard'}), ('english', {'analyzer': 'english'}),
    ]
    cases = []
    for cname, text in crafted:
        SHARED_TEXTS[cname] = text
        for pname, req in plans:
            cases.append({'name': f'crafted/{cname}/{pname}', 'compact': True, 'request': dict(req, text={'$text': cname})})
    for i in range(16):
        text = fuzz_text(rng, word_pools + ['url', 'html', 'thai'], rng.randint(1500, 3000))
        pname, req = plans[i % len(plans)]
        cases.append({'name': f'fuzz/{i}/{pname}', 'compact': True, 'request': dict(req, text=text)})
    return cases


@group('request')
def request_parsing():
    """How `_analyze` parses and validates its request body."""
    bodies = [
        ('unknown_field', {'text': 'x', 'foo': 1}),
        ('text_number', {'text': 12}),
        ('text_bool', {'text': True}),
        ('text_null', {'text': None}),
        ('text_object', {'text': {'a': 1}}),
        ('text_list_numbers', {'text': [1, 2]}),
        ('text_list_mixed', {'text': ['a', 1, None]}),
        ('text_nested_list', {'text': [['a']]}),
        ('text_empty_list', {'text': []}),
        ('text_empty', {'text': ''}),
        ('text_empty_strings', {'text': ['', '', 'a', '']}),
        ('analyzer_number', {'text': 'x', 'analyzer': 5}),
        ('analyzer_list', {'text': 'x', 'analyzer': ['standard']}),
        ('analyzer_null', {'text': 'x', 'analyzer': None}),
        ('analyzer_empty', {'text': 'x', 'analyzer': ''}),
        ('tokenizer_number', {'text': 'x', 'tokenizer': 5}),
        ('tokenizer_list', {'text': 'x', 'tokenizer': ['standard']}),
        ('tokenizer_empty_obj', {'text': 'x', 'tokenizer': {}}),
        ('filter_string', {'text': 'Ab', 'tokenizer': 'standard', 'filter': 'lowercase'}),
        ('filter_number', {'text': 'Ab', 'tokenizer': 'standard', 'filter': [5]}),
        ('filter_null', {'text': 'Ab', 'tokenizer': 'standard', 'filter': [None]}),
        ('filter_nested', {'text': 'Ab', 'tokenizer': 'standard', 'filter': [['lowercase']]}),
        ('filter_obj_no_type', {'text': 'Ab', 'tokenizer': 'standard', 'filter': [{'foo': 'bar'}]}),
        ('char_filter_string', {'text': '<b>Ab</b>', 'tokenizer': 'standard', 'char_filter': 'html_strip'}),
        ('char_filter_obj_no_type', {'text': 'Ab', 'tokenizer': 'standard', 'char_filter': [{}]}),
        ('explain_string', {'text': 'x', 'explain': 'true'}),
        ('explain_yes', {'text': 'x', 'explain': 'yes'}),
        ('explain_number', {'text': 'x', 'explain': 1}),
        ('explain_null', {'text': 'x', 'explain': None}),
        ('attributes_string', {'text': 'x', 'explain': True, 'attributes': 'keyword'}),
        ('attributes_without_explain', {'text': 'running', 'tokenizer': 'standard', 'filter': ['porter_stem'], 'attributes': ['keyword']}),
        ('attributes_empty', {'text': 'running', 'tokenizer': 'standard', 'filter': ['porter_stem'], 'explain': True, 'attributes': []}),
        ('attributes_several', {'text': 'running', 'tokenizer': 'standard', 'filter': ['keyword_repeat', 'porter_stem', 'type_as_payload'],
                                'explain': True, 'attributes': ['keyword', 'payload', 'nope', 'positionLength']}),
        ('normalizer_and_analyzer', {'text': 'x', 'normalizer': 'lowercase', 'analyzer': 'standard'}),
        ('normalizer_and_tokenizer', {'text': 'x', 'normalizer': 'lowercase', 'tokenizer': 'standard'}),
        ('field_and_analyzer', {'text': 'x', 'field': 'f', 'analyzer': 'standard'}),
        ('field_and_tokenizer', {'text': 'x', 'field': 'f', 'tokenizer': 'standard'}),
        ('char_filter_without_tokenizer', {'text': '<b>X</b>', 'char_filter': ['html_strip']}),
        ('filter_with_analyzer', {'text': 'X', 'analyzer': 'standard', 'filter': ['lowercase']}),
        ('char_filter_with_analyzer', {'text': 'X', 'analyzer': 'standard', 'char_filter': ['html_strip']}),
        ('empty_body', {}),
        ('only_explain', {'explain': True}),
        ('explain_analyzer', {'text': ['Quick fox', 'jumps'], 'analyzer': 'english', 'explain': True}),
        ('explain_standard', {'text': 'Quick', 'analyzer': 'standard', 'explain': True}),
        ('explain_keyword', {'text': 'Quick', 'analyzer': 'keyword', 'explain': True}),
        ('explain_normalizer_like', {'text': ['ÀB', 'c'], 'filter': ['lowercase', 'asciifolding'], 'explain': True}),
        ('explain_empty_text', {'text': '', 'analyzer': 'standard', 'explain': True}),
        ('explain_empty_text_custom', {'text': ['', 'a'], 'tokenizer': 'standard', 'explain': True}),
        ('index_param_only', {'text': 'x', 'index': 'foo'}),
        ('tokenizer_and_analyzer_empty', {'text': 'x', 'tokenizer': '', 'analyzer': ''}),
        ('long_text_list', {'text': ['a b'] * 50, 'tokenizer': 'whitespace'}),
    ]
    cases = [{'name': n, 'request': b} for n, b in bodies]
    # The token count limit: 10000 tokens by default.
    cases.append({'name': 'limit/tokens', 'compact': True, 'request': {'tokenizer': 'whitespace', 'text': 'a ' * 10001}})
    cases.append({'name': 'limit/tokens_ok', 'compact': True, 'request': {'tokenizer': 'whitespace', 'text': 'a ' * 10000}})
    cases.append({'name': 'limit/index', 'index': {'analyze': {'max_token_count': 3}}, 'request': {'tokenizer': 'whitespace', 'text': 'a b c d'}})
    cases.append({'name': 'limit/index_explain', 'index': {'analyze': {'max_token_count': 3}},
                  'request': {'tokenizer': 'whitespace', 'explain': True, 'text': 'a b c'}})
    return cases


@group('index3')
def index3():
    """Index analysis: default analyzers, gaps on every analyzer type, chains that refer to
    configured components, and synonym rules analyzed by the chain before them."""
    T = ['The Quick foxes', 'jumped over']
    settings_cases = [
        ('defaults', {'analysis': {'analyzer': {'default': {'type': 'whitespace'}, 'default_search': {'type': 'keyword'},
                                                'default_search_quoted': {'type': 'simple'}}}},
         [{'text': T}, {'analyzer': 'default', 'text': T}, {'analyzer': 'default_search', 'text': T},
          {'analyzer': 'default_search_quoted', 'text': T}, {'text': T, 'explain': True}]),
        ('default_custom', {'analysis': {'analyzer': {'default': {'tokenizer': 'standard', 'filter': ['uppercase'], 'position_increment_gap': 7}}}},
         [{'text': T}, {'text': T, 'explain': True}]),
        ('gaps', {'analysis': {'analyzer': {
            'std': {'type': 'standard', 'position_increment_gap': 5}, 'ws': {'type': 'whitespace', 'position_increment_gap': 5},
            'en': {'type': 'english', 'position_increment_gap': 5}, 'pat': {'type': 'pattern', 'position_increment_gap': 5},
            'cu': {'tokenizer': 'standard', 'position_increment_gap': 0, 'offset_gap': 10},
            'cu2': {'tokenizer': 'standard', 'offset_gap': 0}, 'kw': {'type': 'keyword', 'position_increment_gap': 2}}}},
         [{'analyzer': a, 'text': T} for a in ['std', 'ws', 'en', 'pat', 'cu', 'cu2', 'kw', 'standard']]
         + [{'analyzer': a, 'text': T, 'explain': True} for a in ['std', 'cu', 'kw']]),
        ('refs', {'analysis': {
            'filter': {'my_stem': {'type': 'stemmer', 'language': 'light_english'},
                       'syn': {'type': 'synonym', 'synonyms': ['Foxes, Hounds', 'jumping => leaping']},
                       'syn_graph': {'type': 'synonym_graph', 'synonyms': ['quick fox, qf', 'Jumped => vaulted']},
                       'mx': {'type': 'multiplexer', 'filters': ['my_stem', 'lowercase, my_stem', 'syn']},
                       'cond': {'type': 'condition', 'filter': ['my_stem', 'uppercase'], 'script': {'source': 'true'}},
                       'wdg': {'type': 'word_delimiter_graph', 'preserve_original': True}},
            'char_filter': {'amp': {'type': 'mapping', 'mappings': ['& => and']}},
            'tokenizer': {'ws': {'type': 'whitespace', 'max_token_length': 5}},
            'analyzer': {
                'a1': {'tokenizer': 'standard', 'filter': ['lowercase', 'my_stem', 'syn']},
                'a2': {'tokenizer': 'standard', 'filter': ['my_stem', 'syn_graph']},
                'a3': {'tokenizer': 'ws', 'char_filter': ['amp'], 'filter': ['mx']},
                'a4': {'tokenizer': 'standard', 'filter': ['cond', 'syn']},
                'a5': {'tokenizer': 'whitespace', 'filter': ['wdg', 'flatten_graph']},
                'a6': {'tokenizer': 'standard', 'filter': ['lowercase', 'syn_graph']}}}},
         [{'analyzer': a, 'text': t} for a in ['a1', 'a2', 'a3', 'a4', 'a5', 'a6']
          for t in ['The quick Foxes jumped & jumping', 'quick fox QF hounds-foxes wi-fi']]
         + [{'analyzer': a, 'text': 'quick fox jumped', 'explain': True} for a in ['a2', 'a3', 'a4', 'a6']]
         + [{'tokenizer': 'ws', 'filter': ['syn', 'my_stem'], 'char_filter': ['amp'], 'text': 'Foxes & foxes'},
            {'tokenizer': 'standard', 'filter': ['lowercase', 'syn_graph'], 'text': 'quick fox'},
            {'tokenizer': 'standard', 'filter': ['mx'], 'text': 'Jumping Foxes', 'explain': True}]),
        ('normalizers', {'analysis': {
            'char_filter': {'m': {'type': 'mapping', 'mappings': ['ß => ss', '- => _']},
                            'pr': {'type': 'pattern_replace', 'pattern': '\\d', 'replacement': '#'}},
            'filter': {'el': {'type': 'elision', 'articles': ['l']}, 'trunc': {'type': 'truncate', 'length': 3}},
            'normalizer': {'n1': {'type': 'custom', 'char_filter': ['m', 'pr'], 'filter': ['lowercase', 'asciifolding', 'el']},
                           'n2': {'type': 'custom', 'filter': ['uppercase', 'cjk_width', 'decimal_digit', 'arabic_normalization',
                                                                'german_normalization', 'scandinavian_folding', 'trim']},
                           'n3': {'type': 'lowercase'}, 'n4': {'filter': ['lowercase']}}}},
         [{'normalizer': n, 'text': t} for n in ['n1', 'n2', 'n3', 'n4']
          for t in ["L'Été Straße-42 ٣ ＡＢ  ", ['Ab', ' cD ']]]
         + [{'normalizer': n, 'text': ['Ab-1', 'Ç'], 'explain': True} for n in ['n1', 'n2', 'n3', 'n4']]),
    ]
    cases = []
    for name, settings, reqs in settings_cases:
        for i, r in enumerate(reqs):
            cases.append({'name': f'{name}/{i}', 'index': settings, 'request': r})
    failing = [
        ('normalizer_trunc', {'analysis': {'filter': {'t': {'type': 'truncate', 'length': 2}}, 'normalizer': {'n': {'filter': ['t']}}}}),
        ('normalizer_tokenizer_setting', {'analysis': {'normalizer': {'n': {'tokenizer': 'keyword', 'filter': ['lowercase']}}}}),
        ('analyzer_self_ref', {'analysis': {'analyzer': {'a': {'tokenizer': 'a'}}}}),
        ('filter_ref_unknown_in_multiplexer_used', {'analysis': {'filter': {'mx': {'type': 'multiplexer', 'filters': ['nope']}},
                                                                 'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['mx']}}}}),
        ('synonym_after_multiplexer', {'analysis': {'filter': {'mx': {'type': 'multiplexer', 'filters': ['lowercase']},
                                                               's': {'type': 'synonym', 'synonyms': ['a, b']}},
                                                    'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['mx', 's']}}}}),
        ('synonym_after_shingle', {'analysis': {'filter': {'s': {'type': 'synonym', 'synonyms': ['a, b']}},
                                                'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['shingle', 's']}}}}),
        ('synonym_after_wdg', {'analysis': {'filter': {'s': {'type': 'synonym_graph', 'synonyms': ['a, b']}},
                                            'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['word_delimiter_graph', 's']}}}}),
        ('synonym_rule_removed_by_stop', {'analysis': {'filter': {'s': {'type': 'synonym', 'synonyms': ['the => a']}},
                                                       'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['stop', 's']}}}}),
        ('synonym_rule_removed_by_stop_lenient', {'analysis': {'filter': {'s': {'type': 'synonym', 'lenient': True, 'synonyms': ['the => a', 'x, y']}},
                                                               'analyzer': {'a': {'tokenizer': 'standard', 'filter': ['stop', 's']}}}}),
        ('gap_not_int', {'analysis': {'analyzer': {'a': {'tokenizer': 'standard', 'position_increment_gap': 'x'}}}}),
        ('position_offset_gap', {'analysis': {'analyzer': {'a': {'tokenizer': 'standard', 'position_offset_gap': 3}}}}),
        ('analyzer_alias', {'analysis': {'analyzer': {'a': {'tokenizer': 'standard', 'alias': ['b']}}}}),
        ('char_filter_unknown_type', {'analysis': {'char_filter': {'c': {'type': 'nope'}}}}),
        ('tokenizer_no_type', {'analysis': {'tokenizer': {'t': {'pattern': 'x'}}}}),
        ('normalizer_no_type_unknown', {'analysis': {'normalizer': {'n': {'filter': ['lowercase'], 'char_filter': ['nope']}}}}),
        ('default_search_only', {'analysis': {'analyzer': {'default_search': {'type': 'keyword'}}}}),
        ('default_search_quoted_only', {'analysis': {'analyzer': {'default_search_quoted': {'type': 'keyword'}}}}),
    ]
    for name, settings in failing:
        cases.append({'name': f'fail/{name}', 'index': settings, 'request': {'text': 'the a x', 'analyzer': 'a'}})
    return cases


@group('unicode')
def unicode_sweep():
    """Random code points from every plane through the components whose behaviour comes from
    Unicode properties (word breaks, letters, case mappings, digits, widths, scripts), to catch
    differences between our tables and the Unicode versions of Lucene's JFlex grammars and the
    JDK. Texts are shared; responses are digests."""
    import random
    rng = random.Random(0x10FFFF)
    ranges = [(0x20, 0x7E), (0xA0, 0x24F), (0x250, 0x2FF), (0x300, 0x36F), (0x370, 0x52F), (0x530, 0x5FF), (0x600, 0x6FF),
              (0x700, 0x8FF), (0x900, 0xDFF), (0xE00, 0xEFF), (0xF00, 0x109F), (0x10A0, 0x1FFF), (0x2000, 0x2BFF),
              (0x2C00, 0x2FFF), (0x3000, 0x33FF), (0x3400, 0x4DBF), (0x4E00, 0x9FFF), (0xA000, 0xABFF), (0xAC00, 0xD7FF),
              (0xF900, 0xFFFD), (0x10000, 0x1FFFF), (0x20000, 0x2FFFF), (0x30000, 0x323AF), (0xE0000, 0xE01EF)]
    keys = []
    for i in range(48):
        out = []
        lo, hi = ranges[i % len(ranges)]
        for _ in range(500):
            r = rng.random()
            if r < 0.12:
                out.append(' ')
            elif r < 0.2:
                lo2, hi2 = rng.choice(ranges)
                out.append(chr(rng.randint(lo2, hi2)))
            else:
                out.append(chr(rng.randint(lo, hi)))
        text = ''.join(c for c in out if not (0xD800 <= ord(c) <= 0xDFFF))
        SHARED_TEXTS[f'u{i}'] = text
        keys.append(f'u{i}')
    plans = [
        ('standard', {'tokenizer': 'standard'}), ('classic', {'tokenizer': 'classic'}), ('uax', {'tokenizer': 'uax_url_email'}),
        ('letter', {'tokenizer': 'letter'}), ('whitespace', {'tokenizer': 'whitespace'}), ('lowercase_tk', {'tokenizer': 'lowercase'}),
        ('lower', {'tokenizer': 'whitespace', 'filter': ['lowercase']}), ('upper', {'tokenizer': 'whitespace', 'filter': ['uppercase']}),
        ('greek_lower', {'tokenizer': 'whitespace', 'filter': [{'type': 'lowercase', 'language': 'greek'}]}),
        ('turkish_lower', {'tokenizer': 'whitespace', 'filter': [{'type': 'lowercase', 'language': 'turkish'}]}),
        ('fold', {'tokenizer': 'whitespace', 'filter': ['asciifolding']}), ('width', {'tokenizer': 'whitespace', 'filter': ['cjk_width']}),
        ('digits', {'tokenizer': 'whitespace', 'filter': ['decimal_digit']}),
        ('wdg', {'tokenizer': 'whitespace', 'filter': [{'type': 'word_delimiter_graph', 'catenate_all': True}]}),
        ('ngram_chars', {'tokenizer': {'type': 'ngram', 'min_gram': 2, 'max_gram': 2, 'token_chars': ['letter', 'digit']}}),
        ('ngram_punct', {'tokenizer': {'type': 'edge_ngram', 'min_gram': 1, 'max_gram': 1, 'token_chars': ['punctuation', 'symbol', 'whitespace']}}),
        ('char_group', {'tokenizer': {'type': 'char_group', 'tokenize_on_chars': ['punctuation', 'symbol', 'whitespace', 'digit']}}),
        ('regex_w', {'tokenizer': {'type': 'pattern', 'pattern': '\\w+', 'group': 0, 'flags': 'UNICODE_CHARACTER_CLASS'}}),
        ('regex_l', {'tokenizer': {'type': 'pattern', 'pattern': '[\\p{L}\\p{M}]+', 'group': 0}}),
        ('regex_ci', {'tokenizer': 'keyword', 'filter': [{'type': 'pattern_replace', 'pattern': '[a-zà-ÿ]', 'replacement': '_',
                                                          'flags': 'CASE_INSENSITIVE|UNICODE_CASE'}]}),
        ('thai', {'tokenizer': 'thai'}), ('cjk', {'tokenizer': 'standard', 'filter': ['cjk_bigram']}),
        ('norm', {'tokenizer': 'whitespace', 'filter': ['arabic_normalization', 'persian_normalization', 'indic_normalization',
                                                         'hindi_normalization', 'bengali_normalization', 'sorani_normalization']}),
        ('euro_norm', {'tokenizer': 'whitespace', 'filter': ['german_normalization', 'scandinavian_folding', 'serbian_normalization']}),
        ('html', {'tokenizer': 'whitespace', 'char_filter': ['html_strip']}),
    ]
    cases = []
    for k in keys:
        for pname, req in plans:
            cases.append({'name': f'{pname}/{k}', 'compact': True, 'request': dict(req, text={'$text': k})})
    return cases


@group('unicode2')
def unicode_sweep2():
    """Words of many languages with supplementary characters, combining marks and format
    characters spliced in, through the token filters that measure or rewrite terms in UTF-16
    units (stemmers, normalizers, length/truncate, reverse, n-grams, fingerprint)."""
    import random
    rng = random.Random(0x1F600)
    words = []
    for t in STEM_TEXTS.values():
        words.extend(t.replace(';', ' ').split())
    extras = ['😀', '𠀀', '𝒜', '\U000E0041', '\u200d', '\u200c', '\u0301', '\u00ad', 'ﬁ', 'İ', 'ẞ', '\U0001D7CE', '𐐷', '𞤢']
    keys = []
    for i in range(24):
        out = []
        for _ in range(120):
            w = rng.choice(words)
            if rng.random() < 0.5:
                pos = rng.randint(0, len(w))
                w = w[:pos] + rng.choice(extras) + w[pos:]
            if rng.random() < 0.2:
                w = w + rng.choice(extras)
            out.append(w)
        SHARED_TEXTS[f'w{i}'] = ' '.join(out)
        keys.append(f'w{i}')
    filters = ['arabic_stem', 'persian_stem', 'czech_stem', 'brazilian_stem', 'french_stem', 'german_stem', 'dutch_stem', 'russian_stem',
               'porter_stem', 'kstem', 'german_normalization', 'hindi_normalization', 'indic_normalization', 'bengali_normalization',
               'sorani_normalization', 'serbian_normalization', 'arabic_normalization', 'persian_normalization', 'reverse', 'apostrophe',
               'classic', 'elision', 'unique', 'remove_duplicates', 'trim', 'uppercase', 'lowercase', 'asciifolding', 'decimal_digit']
    filters += [{'type': 'stemmer', 'language': l} for l in ['light_english', 'minimal_english', 'possessive_english', 'light_french', 'minimal_french',
                'light_german', 'minimal_german', 'light_italian', 'light_spanish', 'spanish_plural', 'light_portuguese', 'minimal_portuguese',
                'portuguese_rslp', 'minimal_galician', 'galician', 'light_russian', 'light_hungarian', 'light_finnish', 'light_swedish',
                'light_norwegian', 'minimal_norwegian', 'light_nynorsk', 'minimal_nynorsk', 'bulgarian', 'greek', 'hindi', 'bengali',
                'latvian', 'indonesian', 'sorani', 'english', 'german2', 'turkish', 'armenian', 'basque', 'catalan', 'finnish',
                'hungarian', 'irish', 'italian', 'lithuanian', 'norwegian', 'portuguese', 'romanian', 'serbian', 'spanish', 'swedish',
                'estonian', 'danish', 'arabic', 'persian', 'czech', 'dutch']]
    filters += [{'type': 'length', 'min': 3, 'max': 6}, {'type': 'truncate', 'length': 4}, {'type': 'ngram', 'min_gram': 2, 'max_gram': 3},
                {'type': 'edge_ngram', 'min_gram': 1, 'max_gram': 3}, {'type': 'lowercase', 'language': 'greek'},
                {'type': 'lowercase', 'language': 'irish'}, {'type': 'lowercase', 'language': 'turkish'}, 'fingerprint', 'cjk_bigram',
                {'type': 'word_delimiter_graph', 'catenate_all': True, 'preserve_original': True}, {'type': 'shingle', 'max_shingle_size': 3}]
    cases = []
    for fi, f in enumerate(filters):
        fname = f if isinstance(f, str) else f.get('language', f['type'])
        for k in keys[:6] if fi % 2 else keys[6:12]:
            cases.append({'name': f'{fi}-{fname}/{k}', 'compact': True,
                          'request': {'tokenizer': 'whitespace', 'filter': ['lowercase', f], 'text': {'$text': k}}})
    for k in keys[12:]:
        for a in ['english', 'french', 'german', 'russian', 'arabic', 'persian', 'hindi', 'greek', 'cjk', 'czech', 'sorani', 'bengali', 'indonesian']:
            cases.append({'name': f'analyzer-{a}/{k}', 'compact': True, 'request': {'analyzer': a, 'text': {'$text': k}}})
    return cases


@group('stem_vocab')
def stem_vocab():
    """Every `stemmer` language and every language analyzer over 1500 words of the language's
    Snowball test vocabulary (snowball-data, BSD; `$SNOWBALL_VOCAB_DIR/<language>.txt`), in
    upper and mixed case too. Texts are shared; responses are digests."""
    import os, random
    d = os.environ['SNOWBALL_VOCAB_DIR']
    rng = random.Random(1500)
    have = {}
    for lang in sorted(set(STEMMER_LANGS.values())):
        path = os.path.join(d, f'{lang}.txt')
        if not os.path.exists(path):
            continue
        words = [w.strip() for w in open(path, encoding='utf-8') if w.strip()]
        sample = rng.sample(words, min(1500, len(words)))
        sample = [w.upper() if i % 7 == 0 else w.capitalize() if i % 5 == 0 else w for i, w in enumerate(sample)]
        SHARED_TEXTS[lang] = ' '.join(sample)
        have[lang] = True
    cases = []
    for stemmer, lang in STEMMER_LANGS.items():
        if lang in have:
            cases.append({'name': f'stemmer/{stemmer}', 'compact': True,
                          'request': {'tokenizer': 'standard', 'filter': ['lowercase', {'type': 'stemmer', 'language': stemmer}],
                                      'text': {'$text': lang}}})
    for lang in have:
        cases.append({'name': f'analyzer/{lang}', 'compact': True, 'request': {'analyzer': lang, 'text': {'$text': lang}}})
    for f in ['porter_stem', 'kstem', 'german_stem', 'french_stem', 'dutch_stem', 'russian_stem', 'brazilian_stem']:
        lang = {'porter_stem': 'english', 'kstem': 'english', 'german_stem': 'german', 'french_stem': 'french', 'dutch_stem': 'dutch',
                'russian_stem': 'russian', 'brazilian_stem': 'portuguese'}[f]
        cases.append({'name': f'filter/{f}', 'compact': True, 'request': {'tokenizer': 'standard', 'filter': ['lowercase', f], 'text': {'$text': lang}}})
    return cases


@group('paths')
def paths():
    """Every `*_path` setting (files under the node's config directory, which roll does not
    have): the exact error for a missing file, a path escaping the config directory, an empty
    path, and a path given next to the inline setting."""
    comps = [
        ('filter', {'type': 'stop'}, 'stopwords_path'), ('filter', {'type': 'synonym'}, 'synonyms_path'),
        ('filter', {'type': 'synonym_graph'}, 'synonyms_path'), ('filter', {'type': 'keyword_marker'}, 'keywords_path'),
        ('filter', {'type': 'elision'}, 'articles_path'), ('filter', {'type': 'stemmer_override'}, 'rules_path'),
        ('filter', {'type': 'dictionary_decompounder'}, 'word_list_path'),
        ('filter', {'type': 'hyphenation_decompounder', 'word_list': ['a']}, 'hyphenation_patterns_path'),
        ('filter', {'type': 'word_delimiter'}, 'type_table_path'), ('filter', {'type': 'word_delimiter'}, 'protected_words_path'),
        ('filter', {'type': 'word_delimiter_graph'}, 'protected_words_path'), ('filter', {'type': 'keep'}, 'keep_words_path'),
        ('filter', {'type': 'common_grams'}, 'common_words_path'), ('char_filter', {'type': 'mapping'}, 'mappings_path'),
        ('analyzer', {'type': 'english'}, 'stopwords_path'), ('analyzer', {'type': 'standard'}, 'stopwords_path'),
        ('analyzer', {'type': 'english'}, 'stem_exclusion_path'),
        ('filter', {'type': 'keyword_marker', 'keywords': ['a']}, 'keywords_path'),
        ('filter', {'type': 'stop', 'stopwords': ['a']}, 'stopwords_path'),
        ('filter', {'type': 'synonym', 'synonyms': ['a, b']}, 'synonyms_path'),
    ]
    cases = []
    for i, (kind, base, key) in enumerate(comps):
        shapes = [('missing', 'analysis/nope.txt'), ('escape', '../../etc/passwd'), ('absolute', '/etc/passwd'), ('empty', '')]
        if i in (0, 1, 7, 13):
            shapes += [('dot', '.'), ('slashes', 'a//b/'), ('data_dir', '/usr/share/elasticsearch/data'), ('jvm', 'jvm.options'),
                       ('dotdot_in', '/usr/share/elasticsearch/config/../config/x'), ('tmp', '/tmp/x'), ('logs', '/usr/share/elasticsearch/logs/x')]
        for vn, v in shapes:
            d = dict(base)
            d[key] = v
            if kind == 'analyzer':
                req = {'analyzer': 'a', 'text': 'x'}
                idx = {'analysis': {'analyzer': {'a': d}}}
                cases.append({'name': f'{i}/{base["type"]}/{key}/{vn}', 'index': idx, 'request': req})
            else:
                req = {'tokenizer': 'standard', kind: [d], 'text': 'x'}
                cases.append({'name': f'{i}/{base["type"]}/{key}/{vn}', 'request': req})
    return cases


@group('synonyms_fuzz')
def synonyms_fuzz():
    """Seeded random synonym rule sets (Solr and WordNet syntax with escapes, comments, explicit
    mappings, multi-word sides, duplicates) parsed through random preceding chains and applied
    to texts with overlapping matches, as `synonym` and `synonym_graph`, with and without
    `expand` and `lenient`."""
    import random
    rng = random.Random(0x5e)
    vocab = ['a', 'b', 'c', 'dog', 'Dogs', 'cat', 'big', 'apple', 'Running', 'new', 'york', 'ny', 'the', 'é', 'ÉCOLE', 'wi-fi', 'x1', 'u.s.a', 'of']
    def side(n):
        return ' '.join(rng.choice(vocab) for _ in range(n))
    def solr_rule():
        r = rng.random()
        lhs = [side(rng.randint(1, 3)) for _ in range(rng.randint(1, 3))]
        if r < 0.05:
            return '# comment ' + side(2)
        if r < 0.08:
            return ''
        if r < 0.12:
            return lhs[0] + '\\, ' + side(1) + ', ' + side(1)
        if r < 0.15:
            return side(1) + ' \\=> ' + side(1) + ', ' + side(1)
        if r < 0.5:
            rhs = [side(rng.randint(1, 3)) for _ in range(rng.randint(1, 2))]
            return ', '.join(lhs) + rng.choice([' => ', '=>', '  =>  ']) + ', '.join(rhs)
        return rng.choice([', ', ',', ' , ']).join(lhs)
    def wordnet_rules():
        out = []
        for sid in range(rng.randint(1, 3)):
            for k in range(rng.randint(1, 3)):
                w = side(rng.randint(1, 2)).replace("'", "''")
                out.append(f"s({100000001 + sid},{k + 1},'{w}',n,1,0).")
        return out
    chains = [[], ['lowercase'], ['lowercase', {'type': 'stemmer', 'language': 'english'}], ['asciifolding', 'lowercase'],
              [{'type': 'stop', 'stopwords': ['the', 'of']}], ['lowercase', {'type': 'stop', 'stopwords': ['the', 'of']}], ['uppercase']]
    cases = []
    for i in range(400):
        wordnet = rng.random() < 0.2
        rules = wordnet_rules() if wordnet else [solr_rule() for _ in range(rng.randint(1, 5))]
        kind = rng.choice(['synonym', 'synonym_graph', 'synonym_graph'])
        cfg = {'type': kind, 'synonyms': rules}
        if wordnet:
            cfg['format'] = 'wordnet'
        if rng.random() < 0.3:
            cfg['expand'] = False
        if rng.random() < 0.4:
            cfg['lenient'] = rng.random() < 0.7
        chain = list(rng.choice(chains))
        filters = chain + [cfg]
        if kind == 'synonym_graph' and rng.random() < 0.3:
            filters.append('flatten_graph')
        if rng.random() < 0.2:
            filters.append('lowercase')
        text = ' '.join(rng.choice(vocab + ['x', 'new york', 'big apple']) for _ in range(rng.randint(1, 10)))
        tk = rng.choice(['whitespace', 'whitespace', 'standard', 'keyword'])
        cases.append({'name': f'syn/{i}', 'request': {'tokenizer': tk, 'filter': filters, 'text': text, 'explain': i % 5 == 0}})
    return cases


@group('html_fuzz')
def html_fuzz():
    """Seeded random markup through `html_strip` (with and without `escaped_tags`): tags with
    odd attributes and case, unclosed constructs, entities in every form and range, comments,
    CDATA, processing instructions, script and style bodies, block tags that become newlines."""
    import random
    rng = random.Random(0x4e1)
    pieces = ['<b>', '</b>', '<B>', '</B >', '<p>', '</p>', '<P class="x">', '<br>', '<br/>', '<BR />', '<div\n>', '</div>', '<li>', '<h1>',
              '</h1>', '<td>', '<tr>', '<a href="x>y">', "<a href='q\"'>", '<a href=x title=y>', '</a>', '<img src="x" alt="a>b"/>',
              '<script>', '</script>', '<script type="t">if (a<b) {}</script>', '<style>p{}</style>', '<style>', '<!-- c -->', '<!--', '-->',
              '<!---->', '<!-- a -- b -->', '<![CDATA[x<y]]>', '<![CDATA[', ']]>', '<?php echo 1; ?>', '<?xml version="1.0"?>', '<!DOCTYPE html>',
              '<!doctype', '<x:y>', '</x:y>', '<_a>', '<1a>', '< b>', '<b', 'b>', '<', '>', '</', '<>', '</>', '<b c=">', '<b c=\'>',
              '&amp;', '&AMP;', '&amp', '&lt;', '&gt', '&nbsp;', '&nbsp', '&eacute;', '&Eacute', '&#65;', '&#065;', '&#x41;', '&#X41;', '&#x;',
              '&#;', '&#0;', '&#55357;', '&#56832;', '&#x1F600;', '&#x110000;', '&#99999999999;', '&#x263a', '&notanentity;', '&', '&;',
              '&#xD800;', '&quot;', '&apos;', '&copy;', '&hellip;', '&thetasym;', '&zwnj;', 'text', 'more text', ' ', '  ', '\n', '\r\n', '\t',
              'é', '😀', '"', "'", '=', '/', 'x', 'AbC', '1.5', '<span>', '</span>', '<em>', '<blockquote>', '<option>', '<select>', '<textarea>',
              '<noscript>', '</noscript>', '<SCRIPT>', '</SCRIPT>', '<input value="<b>">', '<hr>', '<pre>', '<address>', '<table>']
    escaped = [[], ['b'], ['b', 'a'], ['script'], ['p', 'br', 'B']]
    cases = []
    for i in range(300):
        text = ''.join(rng.choice(pieces) for _ in range(rng.randint(1, 25)))
        cf = {'type': 'html_strip', 'escaped_tags': rng.choice(escaped)} if rng.random() < 0.3 else 'html_strip'
        tk = 'keyword' if i % 2 == 0 else 'standard'
        cases.append({'name': f'html/{i}', 'request': {'tokenizer': tk, 'char_filter': [cf], 'text': text}})
    return cases


@group('search_as_you_type')
def search_as_you_type():
    """`_analyze` on `search_as_you_type` subfields (`._2gram` … `._index_prefix`): fixed-size
    shingles over the field's analyzer (with stop-word gaps and stacked synonyms), their edge
    n-grams, and positions across values."""
    analysis = {'analysis': {
        'filter': {'syn': {'type': 'synonym_graph', 'synonyms': ['ny, new york', 'quick => fast, speedy']},
                   'st': {'type': 'stop', 'stopwords': ['the', 'a', 'of']}},
        'analyzer': {'stop_an': {'tokenizer': 'standard', 'filter': ['lowercase', 'st']},
                     'syn_an': {'tokenizer': 'standard', 'filter': ['lowercase', 'syn']},
                     'gap_an': {'tokenizer': 'whitespace', 'position_increment_gap': 10},
                     'kw_an': {'tokenizer': 'keyword'}}}}
    fields = {'s2': {'type': 'search_as_you_type', 'max_shingle_size': 2},
              's3': {'type': 'search_as_you_type'},
              's4': {'type': 'search_as_you_type', 'max_shingle_size': 4},
              'stop': {'type': 'search_as_you_type', 'analyzer': 'stop_an', 'max_shingle_size': 4},
              'syn': {'type': 'search_as_you_type', 'analyzer': 'syn_an'},
              'gap': {'type': 'search_as_you_type', 'analyzer': 'gap_an'},
              'kw': {'type': 'search_as_you_type', 'analyzer': 'kw_an'},
              'en': {'type': 'search_as_you_type', 'analyzer': 'english'}}
    idx = dict(analysis)
    idx['_mappings'] = {'properties': fields}
    texts = ['The quick brown fox', ['a b c', 'd e', 'f'], ['one', 'two three'], 'quick ny of the city', 'x', '',
             'the of a', 'Running dogs jumped over lazy foxes in New York', ['a', 'b', 'c d e f g']]
    cases = []
    for fname, f in fields.items():
        subs = [f'_{n}gram' for n in range(2, f.get('max_shingle_size', 3) + 1)] + ['_index_prefix']
        for sub in subs:
            for i, t in enumerate(texts):
                cases.append({'name': f'{fname}.{sub}/{i}', 'index': idx, 'request': {'field': f'{fname}.{sub}', 'text': t}})
    return cases


@group('chains_fuzz')
def chains_fuzz():
    """Seeded random analysis chains: a tokenizer, up to two char filters and up to six token
    filters drawn from configured components (graph producers and consumers, stemmers, branch
    filters, filters that drop or stack tokens), over random texts; a third with explain."""
    import os, random
    # CHAINS_SEED / CHAINS_COUNT explore other samples (the committed fixture uses the defaults).
    rng = random.Random(int(os.environ.get('CHAINS_SEED', 0xC4A1)))
    count = int(os.environ.get('CHAINS_COUNT', 700))
    tokenizers = ['standard', 'whitespace', 'letter', 'classic', 'uax_url_email', 'keyword', 'thai',
                  {'type': 'ngram', 'min_gram': 2, 'max_gram': 3, 'token_chars': ['letter', 'digit']},
                  {'type': 'edge_ngram', 'min_gram': 1, 'max_gram': 4}, {'type': 'pattern', 'pattern': '[\\s,]+'},
                  {'type': 'char_group', 'tokenize_on_chars': ['whitespace', '-']}, {'type': 'standard', 'max_token_length': 5}]
    char_filters = ['html_strip', {'type': 'mapping', 'mappings': ['& => and', 'ph => f', 'ß => ss']},
                    {'type': 'pattern_replace', 'pattern': '(\\d)-(\\d)', 'replacement': '$1$2'}]
    filters = ['lowercase', 'uppercase', 'asciifolding', {'type': 'asciifolding', 'preserve_original': True}, 'porter_stem', 'kstem',
               {'type': 'stemmer', 'language': 'light_english'}, {'type': 'stemmer', 'language': 'french'}, 'keyword_repeat',
               'remove_duplicates', 'unique', {'type': 'unique', 'only_on_same_position': True}, 'reverse', 'trim',
               {'type': 'truncate', 'length': 3}, {'type': 'length', 'min': 2, 'max': 5}, {'type': 'limit', 'max_token_count': 4},
               'stop', {'type': 'stop', 'stopwords': ['a', 'the', 'of'], 'ignore_case': True, 'remove_trailing': False},
               {'type': 'keyword_marker', 'keywords': ['running', 'dogs']}, {'type': 'shingle', 'max_shingle_size': 3},
               {'type': 'shingle', 'output_unigrams': False, 'filler_token': 'F'}, {'type': 'edge_ngram', 'min_gram': 1, 'max_gram': 3},
               {'type': 'ngram', 'min_gram': 2, 'max_gram': 2}, 'word_delimiter_graph',
               {'type': 'word_delimiter', 'catenate_all': True, 'preserve_original': True}, 'flatten_graph',
               {'type': 'synonym_graph', 'synonyms': ['ny, new york', 'wi fi, wifi', 'dogs => hounds, canines']},
               {'type': 'synonym', 'synonyms': ['quick, fast', 'a b => c']}, {'type': 'multiplexer', 'filters': ['lowercase', 'porter_stem, reverse']},
               {'type': 'multiplexer', 'filters': ['uppercase'], 'preserve_original': False}, 'cjk_width', 'cjk_bigram',
               {'type': 'cjk_bigram', 'output_unigrams': True}, 'decimal_digit', 'apostrophe', 'classic', 'elision',
               {'type': 'common_grams', 'common_words': ['the', 'of', 'a']}, {'type': 'common_grams', 'common_words': ['the'], 'query_mode': True},
               'fingerprint', {'type': 'min_hash', 'hash_count': 1, 'bucket_count': 2, 'hash_set_size': 1}, 'delimited_payload',
               {'type': 'pattern_capture', 'patterns': ['(\\w)(\\w+)']}, {'type': 'pattern_replace', 'pattern': '[aeiou]', 'replacement': ''},
               {'type': 'keep_types', 'types': ['<NUM>']}, {'type': 'keep_types', 'types': ['<NUM>'], 'mode': 'exclude'},
               {'type': 'keep', 'keep_words': ['quick', 'fox', 'new']}, 'german_normalization', 'scandinavian_folding',
               {'type': 'condition', 'filter': ['uppercase'], 'script': {'source': 'true'}}, {'type': 'predicate_token_filter', 'script': {'source': 'false'}},
               {'type': 'hyphenation_decompounder', 'word_list': ['x'], 'hyphenation_patterns_path': 'x.xml'}]
    pools = ['ascii', 'ascii', 'latin', 'cjk', 'numbers', 'url', 'html', 'emoji']
    words = ['The', 'quick', 'brown', 'fox', 'running', 'dogs', 'new', 'york', 'NY', 'wi-fi', 'Wi Fi', 'PowerShot', "O'Neil's", 'a', 'b',
             'of', 'the', 'café', 'straße', '3.14', '12-34', 'x|1.5', 'l\'avion', '我是', 'ｶﾀｶﾅ', '١٢', 'AT&T', '<b>x</b>']
    cases = []
    for i in range(count):
        req = {'tokenizer': rng.choice(tokenizers)}
        if rng.random() < 0.3:
            req['char_filter'] = rng.sample(char_filters, rng.randint(1, 2))
        req['filter'] = [rng.choice(filters) for _ in range(rng.randint(1, 6))]
        if rng.random() < 0.6:
            text = ' '.join(rng.choice(words) for _ in range(rng.randint(1, 12)))
        else:
            text = fuzz_text(rng, pools, rng.randint(1, 20))
        req['text'] = [text, ' '.join(rng.choice(words) for _ in range(3))] if rng.random() < 0.2 else text
        if i % 3 == 0:
            req['explain'] = True
        cases.append({'name': f'chain/{i}', 'request': req})
    return cases


@group('regressions')
def regressions():
    """Cases random exploration turned up, kept as regressions."""
    cases = [
        # An empty term (elision of a bare article) sorts first in a fingerprint and adds no
        # separator.
        ('fingerprint_empty_term', {'tokenizer': {'type': 'standard', 'max_token_length': 5},
                                    'filter': ['porter_stem', 'elision', 'fingerprint'], 'text': "a café O'Neil's"}),
        ('fingerprint_only_empty', {'tokenizer': 'keyword', 'filter': ['elision', 'fingerprint'], 'text': "l'"}),
        # `char_group` builds its tokenizer with the plain attribute factory: no termFrequency.
        ('char_group_explain', {'tokenizer': {'type': 'char_group', 'tokenize_on_chars': ['-']}, 'filter': ['lowercase'],
                                'text': 'a-B', 'explain': True}),
        ('char_group_explain_attrs', {'tokenizer': {'type': 'char_group', 'tokenize_on_chars': ['-']}, 'filter': ['keyword_repeat'],
                                      'text': 'a-B', 'explain': True, 'attributes': ['termFrequency', 'keyword']}),
    ]
    return [{'name': n, 'request': r} for n, r in cases]


@group('index_fuzz')
def index_fuzz():
    """Seeded random index analysis settings: custom components with random (sometimes
    invalid) parameters, analyzers and normalizers referring to them (sometimes to missing
    ones), language analyzers with options; the index creation result and an `_analyze` through
    one of its analyzers."""
    import os, random
    rng = random.Random(int(os.environ.get('INDEX_FUZZ_SEED', 0x1D7)))
    count = int(os.environ.get('INDEX_FUZZ_COUNT', 300))
    vals = [1, 2, 3, 0, -1, 'x', True, '', ['a', 'b'], 'the', 5, 100]
    filter_types = {'stop': ['stopwords', 'ignore_case'], 'length': ['min', 'max'], 'truncate': ['length'], 'ngram': ['min_gram', 'max_gram'],
                    'edge_ngram': ['min_gram', 'max_gram', 'side'], 'shingle': ['min_shingle_size', 'max_shingle_size', 'output_unigrams'],
                    'synonym': ['synonyms', 'expand', 'lenient'], 'synonym_graph': ['synonyms'], 'word_delimiter_graph': ['catenate_all', 'type_table'],
                    'keep': ['keep_words'], 'keep_types': ['types', 'mode'], 'limit': ['max_token_count'], 'stemmer': ['language'],
                    'snowball': ['language'], 'elision': ['articles'], 'pattern_replace': ['pattern', 'replacement'],
                    'multiplexer': ['filters'], 'condition': ['filter'], 'common_grams': ['common_words'], 'unique': ['only_on_same_position'],
                    'lowercase': ['language'], 'stemmer_override': ['rules'], 'min_hash': ['hash_count', 'bucket_count']}
    tok_types = {'standard': ['max_token_length'], 'ngram': ['min_gram', 'max_gram', 'token_chars'], 'pattern': ['pattern', 'group'],
                 'path_hierarchy': ['delimiter', 'skip'], 'char_group': ['tokenize_on_chars'], 'whitespace': ['max_token_length'], 'keyword': []}
    cf_types = {'mapping': ['mappings'], 'pattern_replace': ['pattern', 'replacement'], 'html_strip': ['escaped_tags']}
    good = {'synonyms': ['a, b', 'c => d'], 'stopwords': ['the'], 'keep_words': ['a'], 'types': ['<ALPHANUM>'], 'language': 'english',
            'articles': ['l'], 'pattern': '\\s+', 'filters': ['lowercase'], 'filter': ['lowercase'], 'common_words': ['the'],
            'rules': ['a => b'], 'mappings': ['a => b'], 'tokenize_on_chars': ['-'], 'token_chars': ['letter'], 'type_table': ['# => ALPHA']}
    cases = []
    for i in range(count):
        an = {}
        def comp(kinds, prefix):
            out = {}
            for k in range(rng.randint(0, 2)):
                ty = rng.choice(list(kinds))
                d = {'type': ty}
                for pn in kinds[ty]:
                    r = rng.random()
                    if r < 0.5 and pn in good:
                        d[pn] = good[pn]
                    elif r < 0.75:
                        d[pn] = rng.choice(vals)
                if ty in ('synonym', 'synonym_graph') and 'synonyms' not in d:
                    d['synonyms'] = good['synonyms']
                if ty == 'condition':
                    d['script'] = {'source': 'true'}
                out[f'{prefix}{k}'] = d
            return out
        filters = comp(filter_types, 'f')
        tokenizers = comp(tok_types, 't')
        cfs = comp(cf_types, 'c')
        if filters: an['filter'] = filters
        if tokenizers: an['tokenizer'] = tokenizers
        if cfs: an['char_filter'] = cfs
        fnames = list(filters) + ['lowercase', 'nope', 'stop', 'porter_stem']
        tnames = list(tokenizers) + ['standard', 'whitespace', 'nope']
        cnames = list(cfs) + ['html_strip', 'nope']
        analyzers = {}
        for k in range(rng.randint(1, 2)):
            if rng.random() < 0.7:
                a = {'tokenizer': rng.choice(tnames), 'filter': rng.sample(fnames, rng.randint(0, 3))}
                if rng.random() < 0.3:
                    a['char_filter'] = rng.sample(cnames, 1)
                if rng.random() < 0.2:
                    a['position_increment_gap'] = rng.choice(vals)
            else:
                a = {'type': rng.choice(['standard', 'english', 'french', 'pattern', 'stop', 'fingerprint', 'simple', 'nope', 'snowball'])}
                if rng.random() < 0.5:
                    a[rng.choice(['stopwords', 'stem_exclusion', 'max_token_length', 'pattern', 'language', 'separator'])] = rng.choice(vals)
            analyzers[f'a{k}'] = a
        an['analyzer'] = analyzers
        if rng.random() < 0.3:
            an['normalizer'] = {'n': {'type': 'custom', 'filter': rng.sample(fnames, rng.randint(0, 2)), 'char_filter': rng.sample(cnames, rng.randint(0, 1))}}
        req = {'analyzer': 'a0', 'text': ['The quick-brown Foxes, a b c d', 'x']}
        if 'normalizer' in an and rng.random() < 0.5:
            req = {'normalizer': 'n', 'text': 'Ab-C'}
        cases.append({'name': f'idx/{i}', 'index': {'analysis': an}, 'request': req})
    return cases


@group('params_unused')
def params_unused():
    """The invalid parameters of `params` on components that are defined in index settings but
    used by no analyzer: Elasticsearch reports the errors its factories raise while being
    configured, while the errors Lucene raises when the stream is created only appear once an
    analyzer uses the component."""
    import json as _json
    adhoc = {c['name']: c for c in _json.load(open(f'{OUT}/params.json'))['cases']}
    cases = []
    for kind, comps in PARAMS.items():
        if kind == 'analyzer':
            continue
        for comp, names in comps.items():
            for pn in names:
                for vn, v in PARAM_VALUES:
                    key = f'{kind}/{comp}/{pn}/{vn}'
                    if adhoc.get(key, {}).get('status', 200) == 200:
                        continue
                    d = dict(PARAM_BASE.get((kind, comp), {}))
                    d['type'] = comp
                    d[pn] = v
                    cases.append({'name': key, 'index': {'analysis': {kind: {'c': d}}}, 'request': {'analyzer': 'standard', 'text': 'x'}})
    return cases


# Building blocks for seeded random texts: each pool stresses a family of rules (UAX#29 word
# breaks, emoji sequences, CJK and SEA scripts, URLs and emails, markup, numbers).
FUZZ_POOLS = {
    'ascii': list("abcXYZ019 .,'-_@:/&?#=+%\"!;()[]") + ['the', 'Foo', 'O\'Neil', 'x86', '3.14', 'a.b', 'wi-fi', 'C++'],
    'latin': list('àéîõüçñßøÆŒǅﬁİıĳ') + ['́', '̈', '­', ' ', '’', 'ʼ', '⁠', '​', '﻿'],
    'scripts': list('ΑλφαΣςКиевЖאבג׳״"مرحبا٣٤۵ह्नিন্দ') + ['‌', '‍', 'ა', 'Ե', 'ܐ', 'ހ', 'ꦲ'],
    'cjk': list('中国人日本語のテキストカタカナひらがな한국어ｶﾀｶﾅＦ１ー〆々〇') + ['゙', '゚', '　', '𠀀', '𪚥', '가', 'ㄱ'],
    'sea': list('ภาษาไทยง่ายລາວខ្មែរမြန်မာ') + ['่', 'ั', '្', '္'],
    'emoji': ['😀', '👍', '\U0001f3fd', '🇭', '🇺', '‍', '️', '⃣', '#', '1', '©', '☕', '🏳', '👨', '👩', '🧑‍💻', '\U000e0067', '⌚', '‼', '🔟'],
    'numbers': list('0123456789٠١٢۳४৫๑๒.,-+eE$€%½²Ⅻ') + ['1,000', '0.5', 'v2', '1st', '٣.٤', '１２'],
    'url': ['http://', 'https://', 'www.', 'example', '.com', '.co.uk', '.org', '/path', '?q=1', '#frag', '@', 'user', '.', ':8080',
            'mailto:', 'a+b', '-', '_', '~', '%20', 'ftp://', 'xn--', 'test', '.museum', '.verylongtldx', ' ', ',', '(', ')', '<', '>', '"'],
    'thai': ['ภาษา', 'ไทย', 'ง่าย', 'นิด', 'เดียว', 'กรุงเทพ', 'มหานคร', 'ประเทศ', 'คน', 'การ', 'ความ', 'ที่', 'และ', 'ใน', 'ของ',
             'เป็น', 'ได้', 'มี', 'ไม่', 'จะ', 'ว่า', 'กับ', 'ให้', 'แล้ว', 'โรงเรียน', 'นักเรียน', 'คอมพิวเตอร์', 'อินเทอร์เน็ต',
             'สวัสดี', 'ขอบคุณ', 'ครับ', 'ค่ะ', 'ๆ', 'ฯ', 'ฯลฯ', 'ล', '๑๒๓', 'ก', 'ข', 'ค', 'ง', '่', '้', 'ั', 'ำ', 'ะ', 'เ', 'แ',
             'ฦ', 'ฺ', '๏', '๚', ' ', ' ', '.', ',', '?', '!', '\u200b', 'a', 'B', '1', '\u0301', '\u00ad', '"', '(', ')', '\n'],
    'html': ['<b>', '</b>', '<p>', '</p>', '<br/>', '<br>', '&amp;', '&lt;', '&#65;', '&#x263a;', '&nbsp;', '&bogus;', '&', '<', '>',
             '<!--', '-->', '<script>', '</script>', '<a href="x">', '</a>', '<![CDATA[', ']]>', '<?x ?>', '<!DOCTYPE x>', 'text',
             ' ', '\n', '"', "'", '=', '<style>', '</style>', '<i ', 'x=y>', '<div>', '</div>', 'é', '&eacute', '&#', '&#xZ;'],
}


def fuzz_text(rng, pools, n):
    out = []
    for _ in range(n):
        pool = FUZZ_POOLS[rng.choice(pools)]
        out.append(rng.choice(pool))
        if rng.random() < 0.25:
            out.append(' ')
    return ''.join(out)


@group('fuzz')
def fuzz():
    """Seeded random texts through the main components: a differential check of the rules
    the hand-written matrix does not reach."""
    import random
    rng = random.Random(20261020)
    cases = []
    word_pools = ['ascii', 'latin', 'scripts', 'cjk', 'sea', 'emoji', 'numbers']
    plans = [
        ('standard', {'tokenizer': 'standard'}, word_pools, 60),
        ('standard_max4', {'tokenizer': {'type': 'standard', 'max_token_length': 4}}, word_pools, 15),
        ('classic', {'tokenizer': 'classic'}, word_pools + ['url'], 40),
        ('uax_url_email', {'tokenizer': 'uax_url_email'}, ['ascii', 'url', 'url', 'url', 'latin', 'cjk', 'emoji', 'numbers'], 60),
        ('letter', {'tokenizer': 'letter'}, word_pools, 15),
        ('lowercase_tk', {'tokenizer': 'lowercase'}, word_pools, 15),
        ('whitespace', {'tokenizer': 'whitespace'}, word_pools, 10),
        ('thai_nonthai', {'tokenizer': 'thai'}, ['ascii', 'latin', 'scripts', 'cjk', 'emoji', 'numbers'], 25),
        ('standard_an', {'analyzer': 'standard'}, word_pools, 15),
        ('english', {'analyzer': 'english'}, ['ascii', 'latin', 'numbers'], 20),
        ('cjk_an', {'analyzer': 'cjk'}, ['cjk', 'cjk', 'ascii', 'emoji', 'numbers', 'sea'], 30),
        ('cjk_bigram_unigrams', {'tokenizer': 'standard', 'filter': ['cjk_width', {'type': 'cjk_bigram', 'output_unigrams': True}]}, ['cjk', 'cjk', 'ascii', 'latin'], 20),
        ('asciifolding', {'tokenizer': 'whitespace', 'filter': [{'type': 'asciifolding', 'preserve_original': True}]}, ['latin', 'latin', 'scripts', 'numbers', 'emoji'], 15),
        ('lowercase_upper', {'tokenizer': 'whitespace', 'filter': ['lowercase', 'uppercase']}, ['latin', 'scripts', 'ascii'], 10),
        ('wdg', {'tokenizer': 'whitespace', 'filter': [{'type': 'word_delimiter_graph', 'catenate_all': True, 'catenate_words': True, 'catenate_numbers': True, 'preserve_original': True}]}, ['ascii', 'ascii', 'numbers', 'latin', 'scripts'], 40),
        ('wd', {'tokenizer': 'whitespace', 'filter': [{'type': 'word_delimiter', 'catenate_all': True, 'catenate_words': True, 'catenate_numbers': True, 'preserve_original': True}]}, ['ascii', 'ascii', 'numbers', 'latin', 'scripts'], 30),
        ('wdg_flatten', {'tokenizer': 'whitespace', 'filter': [{'type': 'word_delimiter_graph', 'catenate_words': True, 'split_on_case_change': True}, 'flatten_graph']}, ['ascii', 'ascii', 'numbers'], 15),
        ('html_strip', {'tokenizer': 'keyword', 'char_filter': ['html_strip']}, ['html', 'html', 'html', 'ascii', 'latin'], 50),
        ('html_strip_std', {'tokenizer': 'standard', 'char_filter': ['html_strip']}, ['html', 'html', 'ascii', 'latin'], 30),
        ('icu_like_norm', {'tokenizer': 'standard', 'filter': ['lowercase', 'decimal_digit', 'arabic_normalization', 'persian_normalization', 'indic_normalization', 'hindi_normalization', 'bengali_normalization']}, ['scripts', 'scripts', 'numbers'], 20),
        ('scandinavian', {'tokenizer': 'standard', 'filter': ['scandinavian_normalization', 'scandinavian_folding', 'german_normalization']}, ['latin', 'latin', 'ascii'], 15),
        ('kstem', {'tokenizer': 'standard', 'filter': ['lowercase', 'kstem']}, ['ascii', 'latin'], 15),
        ('ngram', {'tokenizer': {'type': 'ngram', 'min_gram': 1, 'max_gram': 2, 'token_chars': ['letter', 'digit', 'symbol']}}, word_pools, 15),
        ('edge_ngram', {'tokenizer': 'standard', 'filter': [{'type': 'edge_ngram', 'min_gram': 1, 'max_gram': 3}]}, word_pools, 15),
        ('pattern_w', {'tokenizer': {'type': 'pattern', 'pattern': '\\W+', 'flags': 'UNICODE_CHARACTER_CLASS'}}, word_pools, 15),
        ('shingle', {'tokenizer': 'standard', 'filter': ['stop', {'type': 'shingle', 'max_shingle_size': 3}]}, ['ascii'], 10),
        ('fingerprint', {'analyzer': 'fingerprint'}, ['ascii', 'latin'], 10),
        ('thai_text', {'tokenizer': 'thai'}, ['thai', 'thai', 'thai', 'ascii', 'numbers'], 120),
        ('thai_mixed', {'tokenizer': 'thai'}, ['thai', 'ascii', 'latin', 'scripts', 'cjk', 'emoji', 'numbers'], 40),
        ('thai_an', {'analyzer': 'thai'}, ['thai', 'thai', 'ascii', 'numbers'], 30),
    ]
    for name, req, pools, count in plans:
        for i in range(count):
            text = fuzz_text(rng, pools, rng.randint(1, 30))
            cases.append({'name': f'{name}/{i}', 'request': dict(req, text=text)})
    # Long texts: the thai tokenizer reads 1024-unit chunks cut after the last whitespace.
    for i in range(12):
        pools = ['thai', 'thai', 'ascii'] if i % 3 else ['thai']
        text = fuzz_text(rng, pools, rng.randint(300, 900))
        if i % 4 == 0:
            text = text.replace(' ', '')
        cases.append({'name': f'thai_long/{i}', 'request': {'tokenizer': 'thai', 'text': text}})
    return cases


def clean(v):
    """Lone UTF-16 surrogates (a token cut through a surrogate pair) become U+FFFD, which is
    also what Lucene indexes for them (`UnicodeUtil.UTF16toUTF8`)."""
    if isinstance(v, str):
        return ''.join('\ufffd' if 0xD800 <= ord(c) <= 0xDFFF else c for c in v)
    if isinstance(v, list):
        return [clean(x) for x in v]
    if isinstance(v, dict):
        return {k: clean(x) for k, x in v.items()}
    return v


def digest(tokens):
    """A long token list as its count, a 64-bit FNV-1a hash of its lines
    `token\tstart\tend\ttype\tposition\tpositionLength\n` (positionLength empty when 1), and its
    first and last tokens for diagnosis."""
    h = 0xcbf29ce484222325
    for t in tokens:
        line = '\t'.join([t['token'], str(t['start_offset']), str(t['end_offset']), t['type'], str(t['position']),
                          str(t.get('positionLength', ''))]) + '\n'
        for b in line.encode('utf-8', 'surrogatepass'):
            h = ((h ^ b) * 0x100000001b3) & 0xffffffffffffffff
    short = lambda ts: [[t['token'][:40], t['start_offset'], t['end_offset'], t['type'], t['position']] for t in ts]
    return {'count': len(tokens), 'fnv': f'{h:016x}', 'head': short(tokens[:8]), 'tail': short(tokens[-3:])}


def run_case(group, i, case):
    idx = None
    if 'index' in case:
        # Every object created on the shared cluster carries the crate's prefix.
        idx = f'analysis-fx-{group}-{i}'
        case['index_name'] = idx
        settings = dict(case['index'])
        mappings = settings.pop('_mappings', None)
        body = {'settings': settings}
        if mappings:
            body['mappings'] = mappings
        call('DELETE', f'/{idx}')
        st, resp = call('PUT', f'/{idx}', body)
        if st >= 300:
            case['create'] = True
            case['status'] = st
            case['response'] = resp
            return case
    path = f'/{idx}/_analyze' if idx else '/_analyze'
    req = dict(case['request'])
    if isinstance(req.get('text'), dict) and '$text' in req['text']:
        req['text'] = SHARED_TEXTS[req['text']['$text']]
    st, resp = call('POST', path, req)
    case['status'] = st
    case['response'] = clean(resp)
    if case.get('compact') and st == 200 and 'tokens' in case['response']:
        case['response'] = digest(case['response']['tokens'])
    if idx:
        call('DELETE', f'/{idx}')
    return case


def write_doc(doc, fh):
    """One case (and one shared text) per line: compact, and diffs stay per case."""
    dump = lambda v: json.dumps(v, ensure_ascii=False, sort_keys=True, separators=(',', ':'))
    fh.write('{"cases": [\n')
    fh.write(',\n'.join(dump(c) for c in doc['cases']))
    fh.write('\n]')
    if 'texts' in doc:
        fh.write(', "texts": {\n')
        fh.write(',\n'.join(f'{dump(k)}: {dump(v)}' for k, v in sorted(doc['texts'].items())))
        fh.write('\n}')
    fh.write('}\n')


def main():
    for name, f in GROUPS.items():
        if ONLY and name not in ONLY:
            continue
        SHARED_TEXTS.clear()
        cases = f()
        out = [run_case(name, i, c) for i, c in enumerate(cases)]
        doc = {'cases': out}
        if SHARED_TEXTS:
            doc['texts'] = dict(SHARED_TEXTS)
        with open(f'{OUT}/{name}.json', 'w', encoding='utf-8') as fh:
            write_doc(doc, fh)
        bad = sum(1 for c in out if c['status'] >= 300)
        print(f'{name}: {len(out)} cases ({bad} errors)')


main()

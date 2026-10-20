-- convert_from / convert_to / convert between UTF8 and the multibyte (and single-byte)
-- encodings, through Postgres's mapping tables, and the direct conversions between them.
SELECT convert_from('\xa4a2'::bytea, 'EUC_JP'), convert_from('\x8fa2af'::bytea, 'EUC_JP'), convert_from('\x8eb1'::bytea, 'EUC_JP');
SELECT convert_from('\x82a0'::bytea, 'SJIS'), convert_from('\xb1'::bytea, 'SJIS'), convert_from('\x8740'::bytea, 'SJIS'), convert_from('\xed40'::bytea, 'SJIS');
SELECT convert_from('\xb0a1'::bytea, 'EUC_CN'), convert_from('\xb0a1'::bytea, 'EUC_KR'), convert_from('\xc4a1'::bytea, 'EUC_TW'), convert_from('\x8ea2a1a1'::bytea, 'EUC_TW');
SELECT convert_from('\xa440'::bytea, 'BIG5'), convert_from('\x8140'::bytea, 'GBK'), convert_from('\x8141'::bytea, 'UHC'), convert_from('\xd065'::bytea, 'JOHAB');
SELECT convert_from('\x81308130'::bytea, 'GB18030'), convert_from('\x8431a439'::bytea, 'GB18030'), convert_from('\x90308130'::bytea, 'GB18030'), convert_from('\xe3329a35'::bytea, 'GB18030'), convert_from('\xa2e3'::bytea, 'GB18030');
SELECT convert_from('\xa4f7'::bytea, 'EUC_JIS_2004'), convert_from('\xabc4'::bytea, 'EUC_JIS_2004'), convert_from('\x82f5'::bytea, 'SHIFT_JIS_2004'), convert_from('\x8fa1a1'::bytea, 'EUC_JIS_2004');
SELECT encode(convert_to('あア漢字', 'EUC_JP'), 'hex'), encode(convert_to('あア漢字', 'SJIS'), 'hex'), encode(convert_to('汉字', 'EUC_CN'), 'hex'), encode(convert_to('한글', 'EUC_KR'), 'hex');
SELECT encode(convert_to('漢字', 'BIG5'), 'hex'), encode(convert_to('漢字', 'EUC_TW'), 'hex'), encode(convert_to('汉字€', 'GBK'), 'hex'), encode(convert_to('똠', 'UHC'), 'hex'), encode(convert_to('한', 'JOHAB'), 'hex');
SELECT encode(convert_to('😀', 'GB18030'), 'hex'), encode(convert_to('€', 'GB18030'), 'hex'), encode(convert_to(U&'\0080', 'GB18030'), 'hex'), encode(convert_to(U&'\FFFF', 'GB18030'), 'hex');
SELECT encode(convert_to('か゚', 'EUC_JIS_2004'), 'hex'), encode(convert_to('か゚か', 'SHIFT_JIS_2004'), 'hex'), encode(convert_to('か', 'EUC_JIS_2004'), 'hex'), encode(convert_to('ǽ̀', 'EUC_JIS_2004'), 'hex');
SELECT encode(convert_to('Ωé€', 'LATIN1'), 'hex');
SELECT encode(convert_to('é€', 'WIN1252'), 'hex'), encode(convert_to('Ж', 'KOI8R'), 'hex'), encode(convert_to('Ж', 'WIN1251'), 'hex'), encode(convert_to('ő', 'LATIN2'), 'hex'), encode(convert_to('ก', 'WIN874'), 'hex');
SELECT convert_from('\xe9'::bytea, 'LATIN1'), convert_from('\x80'::bytea, 'WIN1252'), convert_from('\xc1'::bytea, 'KOI8U'), convert_from('\xa4'::bytea, 'LATIN9');
SELECT convert_from('\x81'::bytea, 'WIN1252');
-- Invalid and untranslatable input.
SELECT convert_from('\xa4'::bytea, 'EUC_JP');
SELECT convert_from('\xa4a2a4'::bytea, 'EUC_JP');
SELECT convert_from('\xa9a1'::bytea, 'EUC_JP');
SELECT convert_from('\x81'::bytea, 'GB18030');
SELECT convert_from('\x8130'::bytea, 'GB18030');
SELECT convert_from('\x8431a530'::bytea, 'GB18030');
SELECT convert_from('\xff'::bytea, 'SJIS');
SELECT convert_from('\x80'::bytea, 'UTF8');
SELECT convert_from('abc\x00'::bytea, 'UTF8');
SELECT convert_from('\xc0'::bytea, 'BIG5');
SELECT convert_to('😀', 'SJIS');
SELECT convert_to('é', 'EUC_KR');
SELECT convert_to('€', 'LATIN1');
SELECT convert_to('😀', 'BIG5');
-- Direct conversions.
SELECT encode(convert('\xa4a2'::bytea, 'EUC_JP', 'SJIS'), 'hex'), encode(convert('\x82a0'::bytea, 'SJIS', 'EUC_JP'), 'hex'), encode(convert('\xa4a2'::bytea, 'EUC_JP', 'MULE_INTERNAL'), 'hex');
SELECT encode(convert('\xc4a1'::bytea, 'EUC_TW', 'BIG5'), 'hex'), encode(convert('\xa440'::bytea, 'BIG5', 'EUC_TW'), 'hex'), encode(convert('\xa440'::bytea, 'BIG5', 'MULE_INTERNAL'), 'hex');
SELECT encode(convert('\xb0a1'::bytea, 'EUC_CN', 'MULE_INTERNAL'), 'hex'), encode(convert('\x93b0a1'::bytea, 'MULE_INTERNAL', 'EUC_KR'), 'hex'), encode(convert('\x92b0a1'::bytea, 'MULE_INTERNAL', 'EUC_JP'), 'hex');
SELECT encode(convert('\xe9'::bytea, 'LATIN1', 'MULE_INTERNAL'), 'hex'), encode(convert('\x81e9'::bytea, 'MULE_INTERNAL', 'LATIN1'), 'hex'), encode(convert('\xc1'::bytea, 'KOI8R', 'WIN1251'), 'hex'), encode(convert('\xa5'::bytea, 'WIN1250', 'LATIN2'), 'hex');
SELECT encode(convert('\xa4f7'::bytea, 'EUC_JIS_2004', 'SHIFT_JIS_2004'), 'hex'), encode(convert('\x82f5'::bytea, 'SHIFT_JIS_2004', 'EUC_JIS_2004'), 'hex');
SELECT encode(convert('\xa4a2'::bytea, 'EUC_JP', 'SQL_ASCII'), 'hex'), encode(convert('\xa4'::bytea, 'SQL_ASCII', 'EUC_JP'), 'hex'), encode(convert('abc'::bytea, 'SQL_ASCII', 'UTF8'), 'hex');
SELECT convert('\xa4a2'::bytea, 'EUC_JP', 'BIG5');
SELECT convert('\xa9a1'::bytea, 'EUC_JP', 'SJIS');
SELECT convert('\x8ea1'::bytea, 'MULE_INTERNAL', 'UTF8');
SELECT convert('\xff'::bytea, 'SQL_ASCII', 'UTF8');
-- Encoding names.
SELECT convert_from('\xe9'::bytea, 'latin1'), convert_from('\xe9'::bytea, 'iso-8859-1'), convert_from('\x82a0'::bytea, 'Shift_JIS');
SELECT convert_from('\xa4f7'::bytea, 'foo');
SELECT convert_to('x', 'foo');
SELECT convert('\xa4f7'::bytea, 'EUC_JP', 'foo');
SELECT convert('\xa4f7'::bytea, 'foo', 'EUC_JP');
-- length in an encoding, and the longest character.
SELECT length('\xa4a2a4a4'::bytea, 'EUC_JP'), length('\x8fa2afa4a2'::bytea, 'EUC_JP'), length('\x81308130a4a2'::bytea, 'GB18030'), length('abc'::bytea, 'LATIN1');
SELECT length('\xa4'::bytea, 'EUC_JP');
SELECT length('x'::bytea, 'nope');
SELECT pg_encoding_max_length(6), pg_encoding_max_length(1), pg_encoding_max_length(39), pg_encoding_max_length(8), pg_encoding_max_length(100);

-- The XML tree and its entry points (agent 107): xmlagg appending in place, input checked
-- without building a tree, the compact tree's links (entity content, namespaces, siblings).

-- xmlagg: the declaration merged as xmlconcat2 does it row by row.
SELECT xmlagg(x ORDER BY i) FROM (VALUES
  (1, '<?xml version="1.1"?><a/>'::xml), (2, '<b/>'::xml)) v(i, x);
SELECT xmlagg(x ORDER BY i) FROM (VALUES
  (1, '<?xml version="1.1" standalone="yes"?><a/>'::xml),
  (2, '<?xml version="1.1" standalone="yes"?><b/>'::xml),
  (3, '<?xml version="1.1" standalone="no"?><c/>'::xml)) v(i, x);
SELECT xmlagg(x ORDER BY i) FROM (VALUES
  (1, '<?xml version="1.0" standalone="yes"?><a/>'::xml),
  (2, '<b/>'::xml), (3, '<?xml version="1.0" standalone="yes"?><c/>'::xml)) v(i, x);
SELECT xmlagg(x ORDER BY i) FROM (VALUES
  (1, '<a/>'::xml), (2, '<?xml version="1.1"?><b/>'::xml), (3, 'text'::xml)) v(i, x);
SELECT xmlagg(x ORDER BY i) FROM (VALUES (1, '<?xml version="1.1"?><only/>'::xml)) v(i, x);
SELECT xmlagg(x ORDER BY i) FROM (VALUES (1, NULL::xml), (2, '<a/>'::xml), (3, NULL)) v(i, x);
SELECT length(xmlagg(xmlelement(name r, i))::text) FROM generate_series(1, 20000) i;
SELECT xmlelement(name rows, xmlagg(xmlelement(name row, xmlattributes(i AS id),
  xmlforest(i AS a, i * 2 AS b)) ORDER BY i)) FROM generate_series(1, 3) i;

-- Input checked without a tree: the same errors, warnings and acceptance.
SELECT '<a><b></a>'::xml;
SELECT '<a>&undefined;</a>'::xml;
SELECT '<!DOCTYPE a [<!ENTITY e "<b>x</b>">]><a>&e;&e;</a>'::xml;
SELECT '<!DOCTYPE a [<!ENTITY e "<b>">]><a>&e;</a>'::xml;
SELECT '<!DOCTYPE a [<!ENTITY e "&e;">]><a>&e;</a>'::xml;
SELECT '<a xmlns:p="u"><p:b p:c="1"/><q:d/></a>'::xml;
SELECT '<a xmlns="x" xmlns="y"/>'::xml;
SELECT '<a b="1" b="2"/>'::xml;
SELECT xmlparse(document '<!DOCTYPE a [<!ELEMENT a (b)*><!ELEMENT b EMPTY>]><a>  <b/>  </a>');
SELECT xmlparse(content '  <a/> text <b/><!-- c --><?pi d?>');
SELECT xmlparse(content '<a>&amp;</a><?xml version="1.0"?>');
SELECT xml_is_well_formed('<a><b/></a>'), xml_is_well_formed('<a><b></a>'),
  xml_is_well_formed_document('<a/><b/>'), xml_is_well_formed_content('<a/><b/>');
SELECT xmlserialize(document '<a><b/></a>' AS text);
SELECT xmlserialize(document '<a/><b/>' AS text);
SELECT xmlserialize(content '<a><b>1</b></a><c/>' AS text indent);
SELECT xmlserialize(document '<!DOCTYPE a [<!ENTITY e "v">]><a>&e;<b/></a>' AS text indent);

-- The tree's links: siblings, attributes, entity content and namespaces through XPath.
SELECT xpath('//b/following-sibling::*', '<a><b/><c/><d/></a>');
SELECT xpath('//d/preceding-sibling::node()', '<a><b/>t<c/><d/></a>');
SELECT xpath('//@*', '<a x="1" y="2"><b z="3"/></a>');
SELECT xpath('/a/@y/following-sibling::node()', '<a x="1" y="2"/>');
SELECT xpath('string(/a)', '<!DOCTYPE a [<!ENTITY e "<b>in</b>t">]><a>x&e;y</a>');
SELECT xpath('/a/node()', '<!DOCTYPE a [<!ENTITY e "<b>in</b>t">]><a>x&e;y</a>');
SELECT xpath('//p:c', '<a xmlns:p="u"><b><p:c>1</p:c></b></a>', ARRAY[ARRAY['p', 'u']]);
SELECT xpath('//c', '<a xmlns:p="u"><b xmlns:q="v"><c p:x="1" q:y="2"/></b></a>');
SELECT xpath('//namespace::*', '<a xmlns:p="u"><b xmlns="v"/></a>');
SELECT * FROM xmltable('/r/x' PASSING '<r xmlns:p="u"><x p:a="1"><p:y>2</p:y></x><x/></r>'
  COLUMNS n FOR ORDINALITY, a text PATH '@*', y xml PATH '*', s xml PATH '.');
SELECT xpath('count(//*)', xmlelement(name a, xmlagg(xmlelement(name b, i)))) FROM generate_series(1, 100) i;

-- xml2's xslt_process (agent 98): the native XSLT 1.0 processor against libxslt 1.1.43,
-- a sample of the differential corpus (templates, modes, sorting, numbering, keys,
-- namespaces, the output methods, errors, parameter strings); and xpath_table outside FROM.
CREATE EXTENSION xml2;
-- apply_001_select_children_order
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:apply-templates select="doc/b|doc/a"/></xsl:template><xsl:template match="*"><xsl:value-of select="name()"/></xsl:template></xsl:stylesheet>$q$);
-- apply_007_select_number_error
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:apply-templates select="1"/></xsl:template></xsl:stylesheet>$q$);
-- attrset_001_on_lre
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><r xsl:use-attribute-sets="s" x="lre"/></xsl:template><xsl:attribute-set name="s"><xsl:attribute name="x">set</xsl:attribute><xsl:attribute name="y">set</xsl:attribute></xsl:attribute-set><xsl:template match="zz"></xsl:template></xsl:stylesheet>$q$);
-- attrset_005_merged_same_name
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><r xsl:use-attribute-sets="s"/></xsl:template><xsl:attribute-set name="s"><xsl:attribute name="a">1</xsl:attribute></xsl:attribute-set><xsl:attribute-set name="s"><xsl:attribute name="b">2</xsl:attribute><xsl:attribute name="a">3</xsl:attribute></xsl:attribute-set><xsl:template match="zz"></xsl:template></xsl:stylesheet>$q$);
-- builtin_001_empty_stylesheet
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"></xsl:stylesheet>$q$);
-- builtin_005_root_builtin_reached
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="b">[B]</xsl:template></xsl:stylesheet>$q$);
-- call_001_defaults_and_override
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:call-template name="t"><xsl:with-param name="b" select="'B'"/></xsl:call-template></xsl:template><xsl:template name="t"><xsl:param name="a" select="'A'"/><xsl:param name="b" select="'b'"/><xsl:param name="c">C<x/></xsl:param><xsl:value-of select="concat($a,$b,$c)"/></xsl:template></xsl:stylesheet>$q$);
-- call_006_undeclared_param_ignored
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:call-template name="t"><xsl:with-param name="z" select="1"/></xsl:call-template></xsl:template><xsl:template name="t">ok</xsl:template></xsl:stylesheet>$q$);
-- cond_001_if_types
SELECT xslt_process($q$<r><e/></r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:if test="0">0</xsl:if><xsl:if test="1">1</xsl:if><xsl:if test="''">e</xsl:if><xsl:if test="'0'">s0</xsl:if><xsl:if test="0 div 0">nan</xsl:if><xsl:if test="r/e">node</xsl:if><xsl:if test="r/z">none</xsl:if><xsl:if test="string(r/e)">str</xsl:if></xsl:template></xsl:stylesheet>$q$);
-- cond_004_rtf_test_true
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:variable name="e"></xsl:variable><xsl:variable name="f"><x/></xsl:variable><xsl:if test="$e">E</xsl:if><xsl:if test="$f">F</xsl:if><xsl:if test="string($f)">S</xsl:if></xsl:template></xsl:stylesheet>$q$);
-- copy_001_shallow_copy_element
SELECT xslt_process($q$<doc xmlns:q="urn:q" q:at="1" plain="2"><q:e>t<!--c--><?pi d?></q:e><f xmlns="urn:d"><g/></f></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><xsl:for-each select="//*"><xsl:copy/></xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- copy_009_copy_of_rtf
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:variable name="v"><x a="1">t</x><!--c-->s</xsl:variable><xsl:template match="/"><r><xsl:copy-of select="$v"/>|<xsl:copy-of select="$v"/></r></xsl:template></xsl:stylesheet>$q$);
-- create_001_element_avt_name
SELECT xslt_process($q$<a n="dyn"/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><xsl:element name="{a/@n}-{1+1}"><xsl:attribute name="{concat('at','tr')}">v</xsl:attribute></xsl:element></xsl:template></xsl:stylesheet>$q$);
-- create_005_element_empty_namespace
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><r xmlns="urn:d"><xsl:element name="e" namespace=""/><xsl:element name="f"/></r></xsl:template></xsl:stylesheet>$q$);
-- create_009_attribute_prefixed_namespace
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><r xmlns:p="urn:old"><xsl:attribute name="p:x" namespace="urn:new">1</xsl:attribute></r></xsl:template></xsl:stylesheet>$q$);
-- create_013_attribute_after_child
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><r><c/><xsl:attribute name="x">1</xsl:attribute></r></xsl:template></xsl:stylesheet>$q$);
-- create_017_attribute_xml_lang
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><r><xsl:attribute name="xml:lang">en</xsl:attribute></r></xsl:template></xsl:stylesheet>$q$);
-- create_021_comment_basic
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><xsl:comment> hello <xsl:value-of select="1+1"/> </xsl:comment><r/></xsl:template></xsl:stylesheet>$q$);
-- err_001_xml_unclosed
SELECT xslt_process($q$<a><b></a>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/">ok</xsl:template></xsl:stylesheet>$q$);
-- err_016_xsl_malformed
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/">$q$);
-- err_031_when_no_test
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:choose><xsl:when>x</xsl:when></xsl:choose></xsl:template></xsl:stylesheet>$q$);
-- err_046_with_param_outside_call
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:with-param name="p" select="1"/>x</xsl:template></xsl:stylesheet>$q$);
-- err_061_bad_avt_expr
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><r x="{1 +}"/></xsl:template></xsl:stylesheet>$q$);
-- err_076_exsl_document_write
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:exsl="http://exslt.org/common" extension-element-prefixes="exsl"><xsl:output method="text"/><xsl:template match="/"><exsl:document href="/tmp/a98_out.txt">data</exsl:document>done</xsl:template></xsl:stylesheet>$q$);
-- exsl_001_exsl_node_set_path
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:exsl="http://exslt.org/common"><xsl:output method="text"/><xsl:template match="/"><xsl:variable name="v"><i>3</i><i>1</i><i>2</i></xsl:variable><xsl:for-each select="exsl:node-set($v)/i"><xsl:sort data-type="number"/><xsl:value-of select="."/></xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- exsl_006_node_set_apply_templates
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:libxslt="http://xmlsoft.org/XSLT/namespace" exclude-result-prefixes="libxslt"><xsl:variable name="v"><i>3</i><i>1</i><i>2</i></xsl:variable><xsl:template match="i"><n><xsl:value-of select=". * 2"/></n></xsl:template><xsl:template match="/"><r><xsl:apply-templates select="libxslt:node-set($v)/i"/></r></xsl:template></xsl:stylesheet>$q$);
-- fmtnum_001_basic_patterns
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="format-number(1234.5, '#,##0.00')"/>|<xsl:value-of select="format-number(0.5, '0.###')"/>|<xsl:value-of select="format-number(0.5, '#.##')"/>|<xsl:value-of select="format-number(12, '000')"/>|<xsl:value-of select="format-number(-12.345, '#.0')"/>|<xsl:value-of select="format-number(1234567.891, '#,###.##')"/>|<xsl:value-of select="format-number(0, '#')"/>|<xsl:value-of select="format-number(0, '0.0')"/></xsl:template></xsl:stylesheet>$q$);
-- fmtnum_003_negative_subpattern
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="format-number(-5, '0;(0)')"/>|<xsl:value-of select="format-number(5, '0;(0)')"/>|<xsl:value-of select="format-number(-5, '0;-0')"/>|<xsl:value-of select="format-number(-0.4, '0')"/>|<xsl:value-of select="format-number(-0.6, '0')"/>|<xsl:value-of select="format-number(-0, '0.0')"/></xsl:template></xsl:stylesheet>$q$);
-- fmtnum_005_rounding_half_even
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="format-number(0.5, '0')"/>|<xsl:value-of select="format-number(1.5, '0')"/>|<xsl:value-of select="format-number(2.5, '0')"/>|<xsl:value-of select="format-number(0.125, '0.00')"/>|<xsl:value-of select="format-number(0.135, '0.00')"/>|<xsl:value-of select="format-number(1.005, '0.00')"/>|<xsl:value-of select="format-number(-2.5, '0')"/>|<xsl:value-of select="format-number(2.675, '0.00')"/></xsl:template></xsl:stylesheet>$q$);
-- fmtnum_007_grouping_odd
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="format-number(123456789, '#,##,###')"/>|<xsl:value-of select="format-number(1234, '#,#')"/>|<xsl:value-of select="format-number(1234, '#,##0,')"/>|<xsl:value-of select="format-number(1234.5678, '0.0#')"/></xsl:template></xsl:stylesheet>$q$);
-- fmtnum_009_decimal_format_named_default
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:decimal-format name="eu" decimal-separator="," grouping-separator="." /><xsl:decimal-format NaN="nan!" infinity="INF" minus-sign="~"/><xsl:template match="/"><xsl:value-of select="format-number(1234.5, '#.##0,00', 'eu')"/>|<xsl:value-of select="format-number(0 div 0, '0')"/>|<xsl:value-of select="format-number(-1 div 0, '0')"/>|<xsl:value-of select="format-number(-3, '0')"/></xsl:template></xsl:stylesheet>$q$);
-- fmtnum_011_undefined_decimal_format
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="format-number(1, '0', 'nope')"/></xsl:template></xsl:stylesheet>$q$);
-- func_001_current_vs_dot
SELECT xslt_process($q$<r><a k="2"/><b n="1"/><b n="2"/></r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="r/a"><xsl:value-of select="count(../b[@n = current()/@k])"/>|<xsl:value-of select="count(../b[@n = ./@k])"/></xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- func_009_unparsed_entity_uri
SELECT xslt_process($q$<!DOCTYPE doc [<!NOTATION gif SYSTEM "image/gif"><!ENTITY pic SYSTEM "pic.gif" NDATA gif>]><doc/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/">[<xsl:value-of select="unparsed-entity-uri('pic')"/>][<xsl:value-of select="unparsed-entity-uri('none')"/>]</xsl:template></xsl:stylesheet>$q$);
-- fwd_001_unknown_instruction_fallback
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="2.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:frobnicate><xsl:fallback>FB</xsl:fallback></xsl:frobnicate></xsl:template></xsl:stylesheet>$q$);
-- fwd_007_fallback_ignored_when_known
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="1"><xsl:fallback>FB</xsl:fallback></xsl:value-of>|<xsl:fallback>top</xsl:fallback></xsl:template></xsl:stylesheet>$q$);
-- html_001_empty_elements
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="html"/><xsl:template match="/"><html><head><title>T</title></head><body><br/><hr/><img src="a.png"/><p></p><input type="checkbox" checked="checked"/><area/><col/><meta name="x" content="y"/><link rel="s"/><base href="/"/><param name="n"/></body></html></xsl:template></xsl:stylesheet>$q$);
-- html_006_uri_attr_escaping
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="html"/><xsl:template match="/"><html><body><a href="a b/é?x=1&amp;y=2">l</a><img src="x y.png"/><form action="é"/><a name="a b é">n</a></body></html></xsl:template></xsl:stylesheet>$q$);
-- html_011_no_head_meta
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="html"/><xsl:template match="/"><html><body>x</body></html></xsl:template></xsl:stylesheet>$q$);
-- html_016_implicit_html_root_namespaced
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><html xmlns="http://www.w3.org/1999/xhtml"><body><br/></body></html></xsl:template></xsl:stylesheet>$q$);
-- html_021_doctype_html_system_only
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="html" doctype-system="about:legacy-compat"/><xsl:template match="/"><html><body/></html></xsl:template></xsl:stylesheet>$q$);
-- html_026_uppercase_elements
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="html"/><xsl:template match="/"><HTML><HEAD><TITLE>t</TITLE></HEAD><BODY><BR/><P/><SCRIPT>a&lt;b</SCRIPT></BODY></HTML></xsl:template></xsl:stylesheet>$q$);
-- key_001_multi_values
SELECT xslt_process($q$<r><p id="1" c="x">a</p><p id="2" c="y">b</p><p id="3" c="x">c</p><q ref="1 3"/><q ref="2"/></r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="key('c','x')"><xsl:value-of select="."/></xsl:for-each></xsl:template><xsl:key name="c" match="p" use="@c"/><xsl:key name="i" match="p" use="@id"/><xsl:template match="zz"></xsl:template></xsl:stylesheet>$q$);
-- key_005_use_multiple_values
SELECT xslt_process($q$<r><p id="1" c="x">a</p><p id="2" c="y">b</p><p id="3" c="x">c</p><q ref="1 3"/><q ref="2"/></r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="key('tok','3')"><xsl:value-of select="name()"/></xsl:for-each>|<xsl:value-of select="count(key('w','b'))"/></xsl:template><xsl:key name="tok" match="q" use="@ref"/><xsl:key name="w" match="p" use="(.|@c)"/><xsl:template match="zz"></xsl:template></xsl:stylesheet>$q$);
-- mode_001_two_modes
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:apply-templates select="doc/a" mode="x"/>|<xsl:apply-templates select="doc/a" mode="y"/>|<xsl:apply-templates select="doc/a"/></xsl:template><xsl:template match="a" mode="x">X</xsl:template><xsl:template match="a" mode="y">Y</xsl:template><xsl:template match="a">-</xsl:template></xsl:stylesheet>$q$);
-- mode_005_prefixed_mode_other_prefix
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:p="urn:p"><xsl:output method="text"/><xsl:template match="/"><xsl:apply-templates select="doc/a" mode="q:m" xmlns:q="urn:p"/></xsl:template><xsl:template match="a" mode="p:m">PM</xsl:template></xsl:stylesheet>$q$);
-- msg_001_message_no_terminate
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/">before<xsl:message>note <xsl:value-of select="1+1"/></xsl:message>after</xsl:template></xsl:stylesheet>$q$);
-- msg_004_message_terminate_in_template
SELECT xslt_process($q$<r><i>1</i><i>stop</i><i>3</i></r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="r/i"><xsl:if test=". = 'stop'"><xsl:message terminate="yes">bad item</xsl:message></xsl:if><xsl:value-of select="."/></xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- ns_001_lre_ns_copied
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:p="urn:p"><xsl:template match="/"><r xmlns:u="urn:u"><u:x/></r></xsl:template></xsl:stylesheet>$q$);
-- ns_004_exclude_default
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns="urn:d" xmlns:p="urn:p" exclude-result-prefixes="#default p"><xsl:template match="/"><r><x/></r></xsl:template></xsl:stylesheet>$q$);
-- ns_007_exclude_undeclared_prefix
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" exclude-result-prefixes="nope"><xsl:template match="/"><r/></xsl:template></xsl:stylesheet>$q$);
-- ns_010_default_ns_undeclaration
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><r xmlns="urn:d"><x xmlns=""><y/></x></r></xsl:template></xsl:stylesheet>$q$);
-- ns_013_prefix_clash_element
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><p:r xmlns:p="urn:one"><xsl:element name="p:x" namespace="urn:two"/></p:r></xsl:template></xsl:stylesheet>$q$);
-- ns_016_xsl_ns_in_output_lre
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><r xmlns:xsl2="http://www.w3.org/1999/XSL/Transform"/></xsl:template></xsl:stylesheet>$q$);
-- number_001_single_default
SELECT xslt_process($q$<doc><s><t>a</t><t>b</t><u/><t>c</t></s><s><t>d</t><s><t>e</t></s></s></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="//t"><xsl:number/>,</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- number_003_any_level
SELECT xslt_process($q$<doc><s><t>a</t><t>b</t><u/><t>c</t></s><s><t>d</t><s><t>e</t></s></s></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="//t|//u"><xsl:number level="any"/>,</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- number_005_single_count_union
SELECT xslt_process($q$<doc><s><t>a</t><t>b</t><u/><t>c</t></s><s><t>d</t><s><t>e</t></s></s></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="//t|//u"><xsl:number count="t|u"/>,</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- number_007_value_rounding
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:number value="2.5"/>|<xsl:number value="3.5"/>|<xsl:number value="-2"/>|<xsl:number value="0 div 0"/>|<xsl:number value="1 div 0"/>|<xsl:number value="'x'"/>|<xsl:number value="1e10"/></xsl:template></xsl:stylesheet>$q$);
-- number_009_format_tokens
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:number value="3" format="(1)"/>|<xsl:number value="3" format="[a]"/>|<xsl:number value="3" format="#1."/>|<xsl:number value="3" format="x"/>|<xsl:number value="3" format=""/>|<xsl:number value="3" format="١"/></xsl:template></xsl:stylesheet>$q$);
-- number_011_format_avt
SELECT xslt_process($q$<doc><s><t>a</t><t>b</t><u/><t>c</t></s><s><t>d</t><s><t>e</t></s></s></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:variable name="f" select="'I'"/><xsl:for-each select="//t"><xsl:number format="{$f}"/>,</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- out_001_xml_default_decl
SELECT xslt_process($q$<r a="1"><x>t</x><y/><z>é &amp; ü</z></r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><xsl:copy-of select="."/></xsl:template></xsl:stylesheet>$q$);
-- out_009_encoding_utf8
SELECT xslt_process($q$<r a="1"><x>t</x><y/><z>é &amp; ü</z></r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output encoding="UTF-8"/><xsl:template match="/"><xsl:copy-of select="."/></xsl:template></xsl:stylesheet>$q$);
-- out_017_encoding_text_latin1
SELECT xslt_process($q$<r>é 日</r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text" encoding="ISO-8859-1"/><xsl:template match="/"><xsl:value-of select="r"/></xsl:template></xsl:stylesheet>$q$);
-- out_025_media_type
SELECT xslt_process($q$<r a="1"><x>t</x><y/><z>é &amp; ü</z></r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output media-type="application/x-foo"/><xsl:template match="/"><xsl:copy-of select="."/></xsl:template></xsl:stylesheet>$q$);
-- out_033_only_text_result_xml
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/">just text &amp; more</xsl:template></xsl:stylesheet>$q$);
-- out_041_indent_with_ws_text
SELECT xslt_process($q$<r>
  <x> a </x>
  <y/>
</r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output indent="yes"/><xsl:template match="/"><xsl:copy-of select="."/></xsl:template></xsl:stylesheet>$q$);
-- param_001_defaults_no_params
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="concat($p,'|',$v,'|',$q)"/></xsl:template><xsl:param name="p" select="'pdef'"/><xsl:variable name="v" select="'vdef'"/><xsl:param name="q">qdef</xsl:param><xsl:template match="zz"></xsl:template></xsl:stylesheet>$q$);
-- param_005_global_uses_param
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="$v"/></xsl:template><xsl:variable name="v" select="concat('[',$p,']')"/><xsl:param name="p" select="'d'"/><xsl:template match="zz"></xsl:template></xsl:stylesheet>$q$, $q$p='given'$q$);
-- params_001_two_strings
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:param name="a" select="'A'"/><xsl:param name="b" select="'B'"/><xsl:template match="/">[<xsl:value-of select="$a"/>][<xsl:value-of select="$b"/>]</xsl:template></xsl:stylesheet>$q$, $q$a='x',b='y'$q$);
-- params_007_empty_name
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:param name="a" select="'A'"/><xsl:param name="b" select="'B'"/><xsl:template match="/">[<xsl:value-of select="$a"/>][<xsl:value-of select="$b"/>]</xsl:template></xsl:stylesheet>$q$, $q$='x'$q$);
-- params_013_space_in_value
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:param name="a" select="'A'"/><xsl:param name="b" select="'B'"/><xsl:template match="/">[<xsl:value-of select="$a"/>][<xsl:value-of select="$b"/>]</xsl:template></xsl:stylesheet>$q$, $q$a='x y'$q$);
-- params_019_xpath_function_unknown
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:param name="a" select="'A'"/><xsl:param name="b" select="'B'"/><xsl:template match="/">[<xsl:value-of select="$a"/>][<xsl:value-of select="$b"/>]</xsl:template></xsl:stylesheet>$q$, $q$a=nope()$q$);
-- params_025_variable_not_overridden
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:variable name="a" select="'var'"/><xsl:template match="/"><xsl:value-of select="$a"/></xsl:template></xsl:stylesheet>$q$, $q$a='param'$q$);
-- params_031_prefixed_name
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:p="urn:x"><xsl:output method="text"/><xsl:param name="p:n" select="'dflt'"/><xsl:template match="/"><xsl:value-of select="$p:n"/></xsl:template></xsl:stylesheet>$q$, $q$p:n='prefixed'$q$);
-- real_001_identity_drop_attr
SELECT xslt_process($q$<cfg><db host="h" password="secret"/><app debug="1" password="x"/></cfg>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="@*|node()"><xsl:copy><xsl:apply-templates select="@*|node()"/></xsl:copy></xsl:template><xsl:template match="@password"/></xsl:stylesheet>$q$);
-- real_008_tokenize_csv_line
SELECT xslt_process($q$<l>a, b,,c ,d</l>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template name="tok"><xsl:param name="s"/><xsl:param name="sep" select="','"/><xsl:if test="string-length($s) &gt; 0"><xsl:variable name="h" select="substring-before(concat($s, $sep), $sep)"/><f><xsl:value-of select="normalize-space($h)"/></f><xsl:call-template name="tok"><xsl:with-param name="s" select="substring-after($s, $sep)"/><xsl:with-param name="sep" select="$sep"/></xsl:call-template></xsl:if></xsl:template><xsl:template match="l"><row><xsl:call-template name="tok"><xsl:with-param name="s" select="."/></xsl:call-template></row></xsl:template></xsl:stylesheet>$q$);
-- real_022_query_to_xml_csv
SELECT xslt_process($q$<table xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">

<row>
  <id>1</id>
  <name>alpha</name>
  <v>2.5</v>
</row>

<row>
  <id>2</id>
  <name>beta &amp; co</name>
  <v xsi:nil="true"/>
</row>

<row>
  <id>3</id>
  <name>gamma</name>
  <v>-1</v>
</row>

</table>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/table"><xsl:for-each select="row[1]/*"><xsl:value-of select="name()"/><xsl:if test="position() != last()">,</xsl:if></xsl:for-each><xsl:text>&#10;</xsl:text><xsl:for-each select="row"><xsl:for-each select="*"><xsl:value-of select="."/><xsl:if test="position() != last()">,</xsl:if></xsl:for-each><xsl:text>&#10;</xsl:text></xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- real_029_recursion_infinite
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:call-template name="r"><xsl:with-param name="n" select="1 div 0"/></xsl:call-template></xsl:template><xsl:template name="r"><xsl:param name="n"/><xsl:if test="$n &gt; 0"><xsl:if test="$n mod 100 = 0"><xsl:value-of select="$n"/>,</xsl:if><xsl:call-template name="r"><xsl:with-param name="n" select="$n - 1"/></xsl:call-template></xsl:if></xsl:template></xsl:stylesheet>$q$);
-- rtf_001_string_ops_on_rtf
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:variable name="v"><i>3</i><i>1</i><i>2</i></xsl:variable><xsl:value-of select="string-length($v)"/>|<xsl:value-of select="substring($v,2)"/>|<xsl:value-of select="translate($v,'123','abc')"/>|<xsl:value-of select="$v * 1"/>|<xsl:value-of select="contains($v, '12')"/></xsl:template></xsl:stylesheet>$q$);
-- rtf_006_rtf_predicate_error
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:variable name="v"><i>3</i><i>1</i><i>2</i></xsl:variable><xsl:value-of select="$v[1]"/></xsl:template></xsl:stylesheet>$q$);
-- simple_001_simplified_lre
SELECT xslt_process($q$<doc><t>Title</t><i>1</i><i>2</i></doc>$q$, $q$<html xsl:version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><head><title><xsl:value-of select="doc/t"/></title></head><body><xsl:for-each select="doc/i"><p><xsl:value-of select="."/></p></xsl:for-each></body></html>$q$);
-- simple_004_simplified_exclude
SELECT xslt_process($q$<doc/>$q$, $q$<out xsl:version="1.0" xsl:exclude-result-prefixes="p" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:p="urn:p"><x/></out>$q$);
-- sort_001_text_ascending
SELECT xslt_process($q$<l><i n="10" s="b">x1</i><i n="9" s="a">x2</i><i n="NaN" s="B">x3</i><i n="1" s="A">x4</i><i n="10" s="b">x5</i><i s="c">x6</i></l>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="l/i"><xsl:sort select="@s"/><xsl:value-of select="."/>,</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- sort_004_text_case_order_lower
SELECT xslt_process($q$<l><i n="10" s="b">x1</i><i n="9" s="a">x2</i><i n="NaN" s="B">x3</i><i n="1" s="A">x4</i><i n="10" s="b">x5</i><i s="c">x6</i></l>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="l/i"><xsl:sort select="@s" case-order="lower-first"/><xsl:value-of select="."/>,</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- sort_007_number_as_text
SELECT xslt_process($q$<l><i n="10" s="b">x1</i><i n="9" s="a">x2</i><i n="NaN" s="B">x3</i><i n="1" s="A">x4</i><i n="10" s="b">x5</i><i s="c">x6</i></l>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="l/i"><xsl:sort select="@n"/><xsl:value-of select="."/>,</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- sort_010_avt_order_type
SELECT xslt_process($q$<l><i n="10" s="b">x1</i><i n="9" s="a">x2</i><i n="NaN" s="B">x3</i><i n="1" s="A">x4</i><i n="10" s="b">x5</i><i s="c">x6</i></l>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:variable name="o" select="'descending'"/><xsl:for-each select="l/i"><xsl:sort select="@n" data-type="{concat('num','ber')}" order="{$o}"/><xsl:value-of select="."/>,</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- sort_013_sort_unicode_and_space
SELECT xslt_process($q$<l><i>é</i><i>e</i><i>z</i><i> a</i><i>Z</i><i>10</i><i>9</i><i></i></l>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="l/i"><xsl:sort select="."/><xsl:value-of select="."/>,</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- sort_016_sort_bad_datatype
SELECT xslt_process($q$<l><i>b</i><i>a</i></l>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="l/i"><xsl:sort select="." data-type="bogus"/><xsl:value-of select="."/>,</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- space_001_none
SELECT xslt_process($q$<doc>
  <a>  <b> x </b>  </a>
  <p:c xmlns:p="urn:p">  <d/>  </p:c>
  <e xml:space="preserve">  <f/>  </e>
</doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="count(//text())"/>:<xsl:for-each select="//text()">[<xsl:value-of select="translate(.,'&#10; ','n_')"/>]</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- space_006_strip_vs_xml_space
SELECT xslt_process($q$<doc>
  <a>  <b> x </b>  </a>
  <p:c xmlns:p="urn:p">  <d/>  </p:c>
  <e xml:space="preserve">  <f/>  </e>
</doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:strip-space elements="*"/><xsl:template match="/"><xsl:value-of select="count(//e//text())"/>:<xsl:for-each select="//e//text()">[<xsl:value-of select="translate(.,'&#10; ','n_')"/>]</xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- tmpl_001_name_beats_star
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="*">[*<xsl:apply-templates select="*"/>]</xsl:template><xsl:template match="a">A</xsl:template></xsl:stylesheet>$q$);
-- tmpl_005_attr_star_vs_name
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:apply-templates select="//@id"/></xsl:template><xsl:template match="@id">I</xsl:template><xsl:template match="@*">S</xsl:template></xsl:stylesheet>$q$);
-- tmpl_009_predicate_beats_name
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:apply-templates select="doc/a"/></xsl:template><xsl:template match="a[1]">A1</xsl:template><xsl:template match="a">A</xsl:template></xsl:stylesheet>$q$);
-- tmpl_013_fractional_priority
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:apply-templates select="doc/*"/></xsl:template><xsl:template match="a" priority="0.4">A4</xsl:template><xsl:template match="a" priority="0.3">A3</xsl:template><xsl:template match="doc/a">DA</xsl:template></xsl:stylesheet>$q$);
-- tmpl_017_abs_path_pattern
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:apply-templates select="//a"/></xsl:template><xsl:template match="/doc/a">ABS</xsl:template><xsl:template match="a">A</xsl:template></xsl:stylesheet>$q$);
-- tmpl_021_not_self_pattern
SELECT xslt_process($q$<doc><a id="1">x</a><b id="2">y<c>z</c></b><!--cm--><?pi data?><a id="3">w</a></doc>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:apply-templates select="doc/*"/></xsl:template><xsl:template match="*[not(self::a)]">X</xsl:template><xsl:template match="*">.</xsl:template></xsl:stylesheet>$q$);
-- valueof_001_numbers_1
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="1.0"/>|<xsl:value-of select="0.5"/>|<xsl:value-of select="-0.5"/>|<xsl:value-of select="1 div 3"/>|<xsl:value-of select="2 div 3"/>|<xsl:value-of select="0.1 + 0.2"/>|<xsl:value-of select="100000000000000000000"/>|<xsl:value-of select="1 div 0"/>|</xsl:template></xsl:stylesheet>$q$);
-- valueof_007_nodeset_first_doc_order
SELECT xslt_process($q$<r><i>1</i><i>2</i></r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="//i[2] | //i[1]"/>|<xsl:value-of select="//nothing"/>|<xsl:value-of select="/"/></xsl:template></xsl:stylesheet>$q$);
-- var_001_global_forward_ref
SELECT xslt_process($q$<a>t</a>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="concat($a,$b,$c)"/></xsl:template><xsl:variable name="a" select="concat($b,'A')"/><xsl:variable name="c" select="'C'"/><xsl:variable name="b" select="$c"/><xsl:template match="zz"></xsl:template></xsl:stylesheet>$q$);
-- var_008_empty_variable
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/">[<xsl:value-of select="$v"/>][<xsl:value-of select="string-length($v)"/>][<xsl:value-of select="boolean($v)"/>]</xsl:template><xsl:variable name="v"/><xsl:template match="zz"></xsl:template></xsl:stylesheet>$q$);
-- ws_001_lre_whitespace_stripped
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><r>
  <a>  </a>
  <b> x </b>
</r></xsl:template></xsl:stylesheet>$q$);
-- ws_005_source_ws_apply
SELECT xslt_process($q$<r>
	<x>a</x>

	<y>b</y>
</r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/></xsl:stylesheet>$q$);
-- xpath_001_string_functions
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="substring('12345', 1.5, 2.6)"/>|<xsl:value-of select="substring('12345', 0, 3)"/>|<xsl:value-of select="substring('12345', 0 div 0, 3)"/>|<xsl:value-of select="substring('12345', 1, 0 div 0)"/>|<xsl:value-of select="substring('12345', -42, 1 div 0)"/>|<xsl:value-of select="substring('12345', -1 div 0, 1 div 0)"/>|<xsl:value-of select="substring('12345', 2)"/></xsl:template></xsl:stylesheet>$q$);
-- xpath_005_string_length_unicode
SELECT xslt_process($q$<a>héllo 日本 𝄞</a>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="string-length(a)"/>|<xsl:value-of select="substring(a, 7, 2)"/>|<xsl:value-of select="translate(a, 'é𝄞', 'eG')"/>|<xsl:value-of select="string-length('𝄞')"/></xsl:template></xsl:stylesheet>$q$);
-- xpath_009_axes
SELECT xslt_process($q$<r><a><b/><c><d/></c></a><e/></r>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:for-each select="//c"><xsl:for-each select="ancestor-or-self::*|following::*|preceding::*|descendant::*"><xsl:value-of select="name()"/></xsl:for-each>|<xsl:value-of select="count(following-sibling::node())"/><xsl:value-of select="count(preceding-sibling::*)"/><xsl:value-of select="name(parent::*)"/><xsl:value-of select="count(self::c)"/><xsl:value-of select="count(self::node()/..)"/></xsl:for-each></xsl:template></xsl:stylesheet>$q$);
-- xpath_013_string_number_compare
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="'1' = 1"/>|<xsl:value-of select="' 1 ' = 1"/>|<xsl:value-of select="'a' = 0 div 0"/>|<xsl:value-of select="'2' &gt; '10'"/>|<xsl:value-of select="'b' &gt; 'a'"/>|<xsl:value-of select="1 = '1.0'"/>|<xsl:value-of select="true() = 2"/>|<xsl:value-of select="false() = 0"/></xsl:template></xsl:stylesheet>$q$);
-- xpath_017_attribute_axis_variants
SELECT xslt_process($q$<r a="1" b="2" xmlns:p="urn:p" p:c="3"/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:p="urn:p"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="count(r/@*)"/>|<xsl:value-of select="count(r/attribute::*)"/>|<xsl:value-of select="count(r/@p:*)"/>|<xsl:value-of select="r/@p:c"/>|<xsl:value-of select="name(r/@*[3])"/>|<xsl:value-of select="count(r/namespace::*)"/></xsl:template></xsl:stylesheet>$q$);
-- xpath_021_wrong_type_count
SELECT xslt_process($q$<a/>$q$, $q$<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="count('a')"/></xsl:template></xsl:stylesheet>$q$);
CREATE TABLE a98_xt (id int, t text);
INSERT INTO a98_xt VALUES (1, '<doc><int>1</int><int>2</int></doc>');
SELECT xpath_table('id', 't', 'a98_xt', '/doc/int', 'true');
SELECT xpath_table('id', 't', 'a98_xt', '/doc/int', 'false') FROM a98_xt WHERE false;
SELECT * FROM xpath_table('id', 't', 'a98_xt', '/doc/int', 'true') AS t(id int, v int);
DROP TABLE a98_xt;
DROP EXTENSION xml2;

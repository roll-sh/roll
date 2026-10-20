-- Documents with an XHTML 1.0 DTD are dumped by libxml's XHTML rules (xhtmlNodeDumpOutput):
-- XMLSERIALIZE ... INDENT and xpath results of the whole document.
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"><html><head><title>t</title></head><body><p>a<br/>b</p><hr/><p/><div name="x"/><a name="n">l</a><img name="i" src="s"/><p lang="en">x</p><p xml:lang="de">y</p><input/><x:br xmlns:x="u"/></body></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"><html xmlns="http://www.w3.org/1999/xhtml"><head></head><body><br/></body></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"><html><head><meta http-equiv="content-type" content="x"/></head><body/></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"><html><head/></html>' as text indent);
select xmlserialize(document '<?xml version="1.0" encoding="ISO-8859-1"?><!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"><html><head/></html>' as text indent);
-- The three XHTML 1.0 DTDs, by public or system id; not XHTML 1.1, Basic or HTML 4.
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "x"><html><body><br/><p/><![CDATA[x]]><area/></body></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "x"><html><body><col/><base/></body></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Frameset//EN" "x"><html><frame/></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html SYSTEM "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"><html><br/></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html SYSTEM "http://www.w3.org/TR/xhtml1/DTD/xhtml1-frameset.dtd"><html><br/></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD HTML 4.01//EN" "x"><html><br/></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML Basic 1.0//EN" "x"><html><br/></html>' as text indent);
-- Content with a DOCTYPE is a document; without INDENT the text is as written.
select xmlserialize(content '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><br/><p/></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><br/><p/></html>' as text);
-- xpath copies the document node (still XHTML), but not other nodes.
select xpath('/', '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><head/><br/><p/><a name="q"/></html>'::xml);
select xpath('/html/br', '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><head/><br/><p/></html>'::xml);
select xpath('/', '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html xmlns="http://www.w3.org/1999/xhtml"><head><title>a</title></head><body><p>x<br/>y</p><p><br/></p></body></html>'::xml);
select xpath('/', '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><head>x</head></html>'::xml);
select xpath('/', '<!DOCTYPE html SYSTEM "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd"><html><head><meta http-equiv="Content-Type" content="q"/></head><br/></html>'::xml);
select xpath('//head', '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><head/></html>'::xml);
-- Formatting: mixed content stays inline; comments, PIs and CDATA aren't indented.
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><body><p><br/><span>x</span></p><p>x &amp; <b/></p></body></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html xmlns:h="http://www.w3.org/1999/xhtml"><h:br/><br xmlns="other"/><br xmlns="http://www.w3.org/1999/xhtml"/><!-- c --><?pi x?></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x" [<!ENTITY e "v">]><html><p>&e;</p><form name="f" id="g"/><map name="m"/></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><head>t<meta http-equiv="x"/></head></html>' as text indent);
-- The Content-Type meta goes into a head that is a child of the root html element.
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><body><head/></body><x:head xmlns:x="u"/></html>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><body><html><head/></html></body>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><head><meta xmlns:q="z" q:http-equiv="CONTENT-TYPE"/></head></html>' as text indent);
-- id copies name (C.8), lang and xml:lang mirror each other (C.7).
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><p name="a&amp;&lt;b" lang="x&quot;y"/><p xml:lang="a" lang="b"/><div xmlns:x="u" x:name="q"/><iframe name="i"/><span name="s"/></html>' as text indent);
select xmlserialize(document '<?xml version="1.0"?><!-- top --><!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><?pi?><html/>' as text indent);
select xmlserialize(document '<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "x"><html><body><div><div><div><link/><meta/><param/><basefont/><isindex/><col/></div></div></div></body></html>' as text indent);

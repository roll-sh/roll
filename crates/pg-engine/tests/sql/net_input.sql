-- inet/cidr input (inet_net_pton): classful cidr, abbreviations, rejections
SELECT '10.1/16'::cidr, '10/8'::cidr, '10.1.2'::cidr, '192.168'::cidr, '128'::cidr, '1.2.3.4'::cidr, '224'::cidr, '200.1'::cidr, '0x0a01'::cidr;
SELECT '10/8'::inet, '1.2.3.4/024'::inet, '010.1.2.3'::inet, '::ffff:1.2.3.4/120'::inet, '::1.2.3'::inet, '00::1'::inet;
SELECT '10.1.2'::inet;
SELECT '1.2.3.4/33'::inet;
SELECT '1.2.3.4/24'::cidr;
SELECT '::1/129'::inet;
SELECT ' 1.2.3.4'::inet;
SELECT '1.2.3.4/'::inet;
SELECT '::1/08'::inet;
SELECT '1.2.3.4.5'::inet;
SELECT '1:2:3:4:5:6:7:8:9'::inet;
SELECT 'garbage'::cidr;
-- output (inet_net_ntop)
SELECT '::2'::inet, '::1.2.3.4'::inet, '::0:2:3'::inet, '1::'::inet, '::'::inet, '1:0:0:2:0:0:0:3'::inet, '1:0:0:2:0:0:3:4'::inet, '::ffff:0:0'::inet, 'FFFF::1'::inet;
SELECT abbrev(c), c::text FROM (VALUES ('10.0.0.0/8'::cidr), ('10.128.0.0/9'), ('0.0.0.0/0'), ('10.1.0.0/24'), ('192.168.1.128/25'), ('2001:db8::/32'), ('2001:db8:0:1::/64'), ('::/0'), ('::ffff:1.2.3.0/120'), ('::1.2.0.0/112'), ('::1.2.3.0/127'), ('8000::/1')) v(c);
-- ordering (network_cmp) and the inet-to-cidr cast
SELECT i FROM (VALUES ('1.2.3.4/16'::inet), ('1.2.0.0/24'), ('1.2.0.0/16'), ('::1'), ('0.0.0.0/0'), ('1.2.3.4'), ('10.0.0.0/8'), ('9.255.0.0/16')) v(i) ORDER BY i;
SELECT '1.2.3.4/16'::inet < '1.2.0.0/24'::inet, '1.2.3.4/24'::inet::cidr, cidr('1.2.3.4/24'::inet), max(i), min(i) FROM (VALUES ('1.2.3.4'::inet), ('::1')) v(i);
CREATE TABLE gx_net (i inet PRIMARY KEY);
INSERT INTO gx_net VALUES ('1.2.3.4/16'), ('1.2.0.0/24'), ('1.2.0.0/16'), ('10.0.0.0/8'), ('9.255.0.0/16');
SELECT i FROM gx_net WHERE i > '1.2.0.0/16' ORDER BY i;
SELECT i FROM gx_net ORDER BY i DESC;
DROP TABLE gx_net;
-- macaddr / macaddr8 input
SELECT '1:2:3:4:5:6'::macaddr, '08-00-2b-01-02-03'::macaddr, '08002b:010203'::macaddr, '08002b-010203'::macaddr, '0800.2b01.0203'::macaddr, '0800-2b01-0203'::macaddr, '08002b010203'::macaddr, ' 08:00:2b:01:02:03 '::macaddr;
SELECT '08:00:2b:01:02:03:04:05'::macaddr8, '08-00-2b-01-02-03-04-05'::macaddr8, '08002b0102030405'::macaddr8, '0800.2b01.0203.0405'::macaddr8, '08:00:2b:01:02:03'::macaddr8, ' 08:00:2b:01:02:03:04:05  '::macaddr8;
SELECT '08:00:2b:01:02:03:04'::macaddr8;
SELECT '08:00-2b:01:02:03'::macaddr8;
SELECT '08:00:2b:01:02:0g'::macaddr;
SELECT '08:00:2b:01:02:100'::macaddr;
SELECT '12:34:56:00:00:00:00:00'::macaddr8::macaddr;
SELECT trunc('12:34:56:78:90:ab:cd:ef'::macaddr8), ~ '12:34:56:78:90:ab:cd:ef'::macaddr8, '12:34:56:78:90:ab:cd:ef'::macaddr8 & 'ff:ff:ff:ff:00:00:00:00', '12:34:56:78:90:ab:cd:ef'::macaddr8 < '12:34:56:78:90:ab:cd:f0';
SELECT pg_typeof('1:2:3:4:5:6'::macaddr8), '{08:00:2b:01:02:03:04:05}'::macaddr8[];

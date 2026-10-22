-- ON CONFLICT DO UPDATE under a rule: the EXCLUDED range table entry qualifies the Conflict
-- Filter's columns and whole rows (rules 544).
CREATE TABLE ga4_hats (hat_name char(10) primary key, hat_color char(10));
CREATE TABLE ga4_hat_data (hat_name char(10), hat_color char(10));
create unique index ga4_hat_data_unique_idx on ga4_hat_data (hat_name COLLATE "C" bpchar_pattern_ops);
CREATE RULE ga4_hat_upsert AS ON INSERT TO ga4_hats
    DO INSTEAD
    INSERT INTO ga4_hat_data VALUES (NEW.hat_name, NEW.hat_color)
        ON CONFLICT (hat_name)
        DO UPDATE
           SET hat_name = ga4_hat_data.hat_name, hat_color = excluded.hat_color
           WHERE excluded.hat_color <>  'forbidden' AND ga4_hat_data.* != excluded.*
        RETURNING *;
explain (costs off) INSERT INTO ga4_hats VALUES ('h8', 'forbidden') RETURNING *;
explain (costs off) INSERT INTO ga4_hat_data VALUES ('h8', 'forbidden') ON CONFLICT (hat_name) DO UPDATE
  SET hat_color = excluded.hat_color WHERE excluded.hat_color <> 'forbidden' AND ga4_hat_data.* != excluded.*;

import {test} from 'node:test';
import assert from 'node:assert/strict';
import {createClient as officialClient} from '@supabase/supabase-js';
import {createClient} from '../../packages/kurabase-js/dist/index.js';
import {startLocal,publishableKey,secretKey,wallet} from '../../scripts/local-stack.mjs';

test('real Anvil: official SDK, independent SDK, RLS, atomicity and replay', {timeout:180000}, async t=>{
  const stack=await startLocal();
  t.after(()=>stack.stop());
  const admin=async(sql,version)=>{
    const r=await fetch(`${stack.adminUrl}/admin/v1/${version?'migrations':'sql'}`,{method:'POST',headers:{apikey:secretKey,Authorization:`Bearer ${secretKey}`,'Content-Type':'application/json'},body:JSON.stringify({sql,version})});
    return {status:r.status,...await r.json()};
  };
  const migrated=await admin(`CREATE TABLE authors (id integer PRIMARY KEY, name text NOT NULL);
    CREATE TABLE posts (id integer PRIMARY KEY, author_id integer REFERENCES authors(id), owner text NOT NULL, title text NOT NULL UNIQUE);
    INSERT INTO authors VALUES (1,'Ada');
    ALTER TABLE posts ENABLE ROW LEVEL SECURITY;
    CREATE POLICY own_posts ON posts FOR ALL TO authenticated USING (owner = auth.uid()) WITH CHECK (owner = auth.uid());`,'202609090001');
  assert.equal(migrated.status,200,JSON.stringify(migrated));
  assert.equal(migrated.revision,1);
  const session=await stack.session(2);
  const uid=wallet(2).address.toLowerCase();
  const db=officialClient(stack.url,publishableKey,{accessToken:async()=>session.access_token,auth:{persistSession:false,autoRefreshToken:false}});
  const own=createClient(stack.url,publishableKey);
  own.auth.setAccessToken(session.access_token);
  await t.test('official insert, representation, filters and nested FK',async()=>{
    const inserted=await db.from('posts').insert({id:1,author_id:1,owner:uid,title:'hello'});
    assert.equal(inserted.error,null,JSON.stringify(inserted));assert.equal(inserted.data,null);
    const selected=await db.from('posts').select('id,title,authors(name)').eq('id',1).single();
    assert.equal(selected.error,null,JSON.stringify(selected));
    assert.deepEqual(selected.data,{id:1,title:'hello',authors:{name:'Ada'}});
    const referenced=await db.from('posts').select('id,author:authors!author_id!inner(name)').eq('authors.name','Ada');
    assert.equal(referenced.error,null,JSON.stringify(referenced));
    assert.deepEqual(referenced.data,[{id:1,author:{name:'Ada'}}]);
    const updated=await db.from('posts').update({title:'world'}).eq('id',1).select('id,title');
    assert.equal(updated.error,null,JSON.stringify(updated));assert.deepEqual(updated.data,[{id:1,title:'world'}]);
  });
  await t.test('independent SDK observes same chain state',async()=>{
    const result=await own.from('posts').select('id,title').eq('id',1);
    assert.equal(result.error,null,JSON.stringify(result));assert.deepEqual(result.data,[{id:1,title:'world'}]);
  });
  await t.test('bulk constraint and RLS failures are atomic',async()=>{
    const before=await stack.contract.revision();
    const duplicate=await db.from('posts').insert([{id:2,owner:uid,title:'duplicate'},{id:3,owner:uid,title:'duplicate'}]);
    assert.ok(duplicate.error);assert.equal(await stack.contract.revision(),before);
    const denied=await db.from('posts').insert({id:4,owner:wallet(3).address.toLowerCase(),title:'stolen'});
    assert.equal(denied.error.code,'RlsViolation');assert.equal(await stack.contract.revision(),before);
    const rows=await db.from('posts').select('*');assert.equal(rows.data.length,1);
  });
  await t.test('identity forgery and undelegated admin access fail',async()=>{
    const envelope=structuredClone(session.envelope);envelope.session.user=wallet(3).address;
    const response=await fetch(`${stack.url}/auth/v1/session`,{method:'POST',headers:{apikey:publishableKey,'Content-Type':'application/json'},body:JSON.stringify(envelope)});
    assert.equal(response.status,401);
    const denied=await fetch(`${stack.url}/admin/v1/catalog`,{headers:{apikey:secretKey}});assert.equal(denied.status,401);
  });
  await t.test('exact count, inclusive pagination, single/maybeSingle and upsert',async()=>{
    const upsert=await db.from('posts').upsert({id:1,author_id:1,owner:uid,title:'updated'}, {onConflict:'id'}).select();
    assert.equal(upsert.error,null,JSON.stringify(upsert));
    const paged=await db.from('posts').select('id',{count:'exact'}).range(0,0);assert.equal(paged.count,1);assert.equal(paged.data.length,1);
    const missing=await db.from('posts').select('*').eq('id',999).maybeSingle();assert.equal(missing.error,null);assert.equal(missing.data,null);
    const singular=await db.from('posts').select('*').eq('id',999).single();assert.ok(singular.error);
  });
  await t.test('gateway restart rebuilds state exclusively from chain',async()=>{
    await stack.restartGateway();
    const result=await db.from('posts').select('title').eq('id',1).single();assert.equal(result.error,null,JSON.stringify(result));assert.equal(result.data.title,'updated');
    const deleted=await db.from('posts').delete().eq('id',1).select();assert.equal(deleted.error,null,JSON.stringify(deleted));assert.equal(deleted.data.length,1);
    const rows=await db.from('posts').select('*');assert.deepEqual(rows.data,[]);
  });
  await t.test('failed migration leaves no marker or partial schema',async()=>{
    const before=await stack.contract.revision();
    const bad=await admin('CREATE TABLE doomed(id integer PRIMARY KEY); INSERT INTO doomed VALUES(1),(1);','202609090002');
    assert.equal(bad.status,409,JSON.stringify(bad));assert.equal(await stack.contract.revision(),before);
    const query=await admin('SELECT * FROM doomed');assert.notEqual(query.status,200);
  });
  await t.test('scalar, table and mutating RPC use same atomic chain path',async()=>{
    const created=await admin(`CREATE FUNCTION add_one(n integer) RETURNS integer LANGUAGE sql AS $$ SELECT n + 1 $$;
      CREATE FUNCTION add_post(p_id integer,p_title text) RETURNS SETOF posts LANGUAGE sql AS $$ INSERT INTO posts(id,owner,title) VALUES(p_id,auth.uid(),p_title) RETURNING * $$;
      CREATE FUNCTION get_posts(minimum integer) RETURNS SETOF posts LANGUAGE sql AS $$ SELECT * FROM posts WHERE id >= minimum $$;
      CREATE FUNCTION fail_post(p_id integer) RETURNS void LANGUAGE sql AS $$ INSERT INTO posts(id,owner,title) VALUES(p_id,auth.uid(),'first'); INSERT INTO posts(id,owner,title) VALUES(p_id,auth.uid(),'second') $$;`);
    assert.equal(created.status,200,JSON.stringify(created));
    const scalar=await db.rpc('add_one',{n:41});assert.equal(scalar.error,null,JSON.stringify(scalar));assert.equal(scalar.data,42);
    const inserted=await db.rpc('add_post',{p_id:10,p_title:'RPC post'}).select('id,title');
    assert.equal(inserted.error,null,JSON.stringify(inserted));assert.deepEqual(inserted.data,[{id:10,title:'RPC post'}]);
    const selected=await db.rpc('get_posts',{minimum:1}).eq('id',10).select('title');assert.equal(selected.error,null,JSON.stringify(selected));assert.deepEqual(selected.data,[{title:'RPC post'}]);
    const before=await stack.contract.revision();const failed=await db.rpc('fail_post',{p_id:11});assert.ok(failed.error);assert.equal(await stack.contract.revision(),before);
  });
  await t.test('advanced relational reads, views and materialized snapshots use the pinned chain projection',async()=>{
    const setup=await admin(`CREATE TABLE scores (id integer PRIMARY KEY, team text NOT NULL, points integer NOT NULL);
      INSERT INTO scores VALUES (1,'red',10),(2,'red',20),(3,'blue',15);
      CREATE VIEW leading_scores AS SELECT team, MAX(points) AS high_score FROM scores GROUP BY team;
      CREATE MATERIALIZED VIEW score_cache AS SELECT team, MAX(points) AS best_score FROM scores GROUP BY team;`);
    assert.equal(setup.status,200,JSON.stringify(setup));
    const complex=await admin(`WITH ranked AS (
        SELECT id, team, points, RANK() OVER (PARTITION BY team ORDER BY points DESC) AS position
        FROM scores
      )
      SELECT team, points, position
      FROM ranked
      WHERE position = 1
      ORDER BY team`);
    assert.equal(complex.status,200,JSON.stringify(complex));
    assert.deepEqual(complex.rows,[{team:'blue',points:15,position:1},{team:'red',points:20,position:1}]);
    const setOperation=await admin('SELECT id FROM scores WHERE id=1 UNION ALL SELECT id FROM scores WHERE id=3 ORDER BY id');
    assert.equal(setOperation.status,200,JSON.stringify(setOperation));assert.deepEqual(setOperation.rows,[{id:1},{id:3}]);
    const view=await admin('SELECT team,high_score FROM leading_scores ORDER BY team');
    assert.equal(view.status,200,JSON.stringify(view));assert.deepEqual(view.rows,[{team:'blue',high_score:15},{team:'red',high_score:20}]);
    const cached=await admin('SELECT team,best_score FROM score_cache ORDER BY team');
    assert.equal(cached.status,200,JSON.stringify(cached));
    assert.deepEqual(cached.rows,[{team:'blue',best_score:15},{team:'red',best_score:20}]);
    const changed=await admin("UPDATE scores SET points=100 WHERE team='blue'");
    assert.equal(changed.status,200,JSON.stringify(changed));
    const stale=await admin('SELECT team,best_score FROM score_cache ORDER BY team');
    assert.deepEqual(stale.rows,[{team:'blue',best_score:15},{team:'red',best_score:20}]);
    const refreshed=await admin('REFRESH MATERIALIZED VIEW score_cache');
    assert.equal(refreshed.status,200,JSON.stringify(refreshed));
    const current=await admin('SELECT team,best_score FROM score_cache ORDER BY team');
    assert.deepEqual(current.rows,[{team:'blue',best_score:100},{team:'red',best_score:20}]);

    const ownPost=await db.from('posts').insert({id:12,owner:uid,title:'visible before policy change'}).select('id,title');
    assert.equal(ownPost.error,null,JSON.stringify(ownPost));
    const createdCache=await admin('CREATE MATERIALIZED VIEW visible_post_cache AS SELECT id,title FROM posts');
    assert.equal(createdCache.status,200,JSON.stringify(createdCache));
    const initiallyVisible=await db.from('visible_post_cache').select('id,title').eq('id',12);
    assert.equal(initiallyVisible.error,null,JSON.stringify(initiallyVisible));
    assert.deepEqual(initiallyVisible.data,[{id:12,title:'visible before policy change'}]);
    const deny=await admin('CREATE POLICY deny_post_cache ON posts AS RESTRICTIVE FOR SELECT TO authenticated USING (false)');
    assert.equal(deny.status,200,JSON.stringify(deny));
    const afterPolicyChange=await db.from('visible_post_cache').select('id,title').eq('id',12);
    assert.equal(afterPolicyChange.error,null,JSON.stringify(afterPolicyChange));
    assert.deepEqual(afterPolicyChange.data,[],'schema/RLS changes must invalidate per-auth materialized values');
  });
});

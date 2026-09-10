import base64, json, os, pathlib, sqlite3, subprocess, tempfile, time, urllib.request
root=pathlib.Path.cwd()
with tempfile.TemporaryDirectory(prefix='curator-rating-smoke-') as tmp:
    data=pathlib.Path(tmp)
    (data/'settings.json').write_text(json.dumps({'oobe_completed':True,'nsfw_filter_enabled':False}))
    env=dict(os.environ, CURATOR_DATA_DIR=str(data))
    with (data/'startup.log').open('w') as log:
        proc=subprocess.Popen([str(root/'curator.exe'),'--no-window'],cwd=root,env=env,stdout=log,stderr=log,creationflags=subprocess.CREATE_NO_WINDOW)
        def request(path, method='GET', body=None):
            payload=None if body is None else json.dumps(body).encode()
            req=urllib.request.Request('http://127.0.0.1:8642'+path,data=payload,method=method,headers={'Content-Type':'application/json'})
            with urllib.request.urlopen(req,timeout=3) as response:
                return json.load(response)
        try:
            for _ in range(50):
                if proc.poll() is not None: raise RuntimeError((data/'startup.log').read_text())
                try: request('/api/settings'); break
                except OSError: time.sleep(.2)
            else: raise RuntimeError('Startup timed out')
            with sqlite3.connect(data/'data.db') as conn:
                conn.execute("INSERT INTO sources(id,name,url,slug,status,added_at) VALUES(1,'Smoke','https://example.test','test','done','2026')")
                conn.execute("INSERT INTO media(id,source_id,filepath,filename,type,added_at,rating,auto_rating,auto_rating_score,rating_source) VALUES(1,1,'test/1.png','1.png','image','2026',4,4,.72,'auto')")
            conn.close()
            queue=request('/api/media?rating_status=needs_review')
            assert len(queue['media'])==1 and queue['media'][0]['rating_reviewed'] is False
            approved=request('/api/media/1/rating/approve','POST')
            assert approved['rating']==4 and approved['auto_rating']==4 and approved['rating_reviewed'] is True
            manual=request('/api/media/1/rating','PUT',{'rating':3})
            assert manual['rating']==3 and manual['auto_rating']==4 and manual['rating_source']=='human'
            assert request('/api/media?rating_status=needs_review')['media']==[]
            assert len(request('/api/media?rating_status=reviewed')['media'])==1
            html=urllib.request.urlopen('http://127.0.0.1:8642/').read().decode()
            assert 'review-ratings-btn' in html
            app=urllib.request.urlopen('http://127.0.0.1:8642/app.js').read().decode()
            assert 'function feedAcquireWakeLock' in app
            print('Release smoke passed: startup, static UI, approval, manual rating, provenance and review filters.')
        finally:
            proc.terminate()
            proc.wait(timeout=10)


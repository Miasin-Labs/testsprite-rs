const http=require('http'),fs=require('fs');
http.createServer((q,r)=>{r.writeHead(200,{'Content-Type':'text/html'});r.end(fs.readFileSync(__dirname+'/index.html'));}).listen(8350,'127.0.0.1',()=>console.log('page on 8350'));

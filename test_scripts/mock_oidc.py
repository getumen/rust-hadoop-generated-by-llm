import http.server
import json
import base64
import time
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import rsa
import jwt # pip install pyjwt cryptography

PORT = 8080
CLIENT_ID = "test-client"
ISSUER = f"http://localhost:{PORT}"

# Generate RSA key for the mock OIDC provider
private_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
public_key = private_key.public_key()

# Construct JWKS
def get_jwks():
    numbers = public_key.public_numbers()
    def to_base64_url(n):
        return base64.urlsafe_b64encode(n.to_bytes((n.bit_length() + 7) // 8, 'big')).decode('utf-8').rstrip('=')

    return {
        "keys": [
            {
                "kty": "RSA",
                "kid": "test-kid",
                "use": "sig",
                "alg": "RS256",
                "n": to_base64_url(numbers.n),
                "e": to_base64_url(numbers.e)
            }
        ]
    }

class MockOidcHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/.well-known/openid-configuration":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({
                "issuer": ISSUER,
                "jwks_uri": f"{ISSUER}/jwks"
            }).encode())
        elif self.path == "/jwks":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(get_jwks()).encode())
        elif self.path == "/token":
            # Generate token endpoint for automated tests
            token = generate_token("user-123", ["tenant-a"])
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(token.encode())
        else:
            self.send_response(404)
            self.end_headers()

def generate_token(sub, groups):
    payload = {
        "sub": sub,
        "aud": CLIENT_ID,
        "iss": ISSUER,
        "exp": int(time.time()) + 3600,
        "iat": int(time.time()),
        "groups": groups
    }
    return jwt.encode(payload, private_key, algorithm="RS256", headers={"kid": "test-kid"})

if __name__ == "__main__":
    import threading
    import sys

    if len(sys.argv) > 1 and sys.argv[1] == "token":
        # Generate token and print it
        print(generate_token("user-123", ["tenant-a"]))
        sys.exit(0)

    server = http.server.HTTPServer(('localhost', PORT), MockOidcHandler)
    print(f"Mock OIDC server started on {ISSUER}")
    server.serve_forever()

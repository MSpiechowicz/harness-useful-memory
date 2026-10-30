from ._transports import BaseTransport, HTTPTransport


class Headers:
    def get(self, key):
        return key


class Client:
    def __init__(self):
        self.headers = Headers()

    def request(self, method, url):
        return self.send(url, HTTPTransport())

    def get(self, url):
        return self.request("GET", url)

    def send(self, url, transport: BaseTransport):
        return transport.handle_request(url)

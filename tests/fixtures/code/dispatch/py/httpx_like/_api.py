from ._client import Client


def request(method, url):
    with Client() as client:
        return client.request(method, url)


def get(url):
    return request("GET", url)


def post(url):
    return request("POST", url)

class BaseTransport:
    def handle_request(self, request):
        raise NotImplementedError

    def close(self):
        pass


class HTTPTransport(BaseTransport):
    def handle_request(self, request):
        return open_connection(request)


class MockTransport(BaseTransport):
    def handle_request(self, request):
        return None


def open_connection(request):
    return request

from fastapi import APIRouter, FastAPI

app = FastAPI()
router = APIRouter(prefix="/items")


def helper():
    """Example in a docstring is not a route:

    @app.get("/not-a-route")
    def nope(): ...
    """
    return 1


@app.get("/health")
async def health():
    return {"ok": True}


@router.get("/{item_id}")
# a comment between decorator and def is legal
def read_item(item_id: int):
    return {"id": item_id}


@router.api_route(
    "/",
    methods=["GET", "POST"],
)
def list_or_create():
    return []

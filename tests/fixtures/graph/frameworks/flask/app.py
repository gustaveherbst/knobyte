from flask import Blueprint, Flask

app = Flask(__name__)
bp = Blueprint("accounts", __name__, url_prefix="/accounts")


@app.route("/")
def index():
    return "ok"


@bp.route("/<int:account_id>", methods=["GET", "DELETE"])
def account(account_id):
    return str(account_id)


@bp.post("/login")
def login():
    return "in"

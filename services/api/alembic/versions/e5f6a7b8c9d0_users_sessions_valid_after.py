"""users.sessions_valid_after (sign out everywhere)

Revision ID: e5f6a7b8c9d0
Revises: d4e5f6a7b8c9
Create Date: 2026-09-25 12:50:00.000000

"""
from typing import Sequence, Union

import sqlalchemy as sa
from alembic import op

# revision identifiers, used by Alembic.
revision: str = "e5f6a7b8c9d0"
down_revision: Union[str, Sequence[str], None] = "d4e5f6a7b8c9"
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None


def upgrade() -> None:
    """Upgrade schema."""
    # Nullable, no default: existing users have never signed out everywhere, so
    # every token they hold stays valid. Adding it is a metadata-only change.
    with op.batch_alter_table("users", schema=None) as batch_op:
        batch_op.add_column(
            sa.Column("sessions_valid_after", sa.DateTime(timezone=True), nullable=True)
        )


def downgrade() -> None:
    """Downgrade schema."""
    with op.batch_alter_table("users", schema=None) as batch_op:
        batch_op.drop_column("sessions_valid_after")

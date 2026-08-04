FROM python:3.12-slim
WORKDIR /app
COPY requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt
COPY main.py openapi_spec.py ./
RUN useradd -u 1000 -m -d /home/broker broker && chown -R 1000:1000 /app
USER 1000
EXPOSE 8080
CMD ["uvicorn", "main:app", "--host", "0.0.0.0", "--port", "8080"]
